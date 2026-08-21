use factory_core::{
    EventEnvelope, FactoryEvent, InputEnvelopeId, InputEnvelopeSnapshot, InputReceipt,
    PROTOCOL_VERSION, ProjectId, WorkCandidateId, WorkCandidateSnapshot, WorkCandidateStatus,
};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::Type,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{MAX_STATE_PAGE, Result, Store, StoreError, append_event};

const MAX_SOURCE_KIND_BYTES: usize = 64;
const MAX_SOURCE_ID_BYTES: usize = 512;
const MAX_DELIVERY_ID_BYTES: usize = 256;
const MAX_SOURCE_REVISION_BYTES: usize = 256;
const MAX_INPUT_CONTENT_BYTES: usize = 64 * 1024;
const MAX_STATUS_REASON_BYTES: usize = 1024;
const STALE_REASON: &str = "superseded by a different source revision";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewInputEnvelope {
    pub project_id: ProjectId,
    pub source_kind: String,
    pub source_id: String,
    pub delivery_id: String,
    pub source_revision: String,
    pub content: String,
    pub expected_current_candidate_id: Option<WorkCandidateId>,
}

impl Store {
    /// Stores one untrusted observation behind the quarantine boundary.
    /// Receipt has no path to task, message, run, Change, or provider state.
    pub fn receive_input(
        &mut self,
        input: NewInputEnvelope,
        now_ms: i64,
    ) -> Result<(InputReceipt, Vec<EventEnvelope>)> {
        validate_input(&input, now_ms)?;
        let content_digest = sha256_hex(input.content.as_bytes());
        let request_digest = request_digest(&input, &content_digest);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let project_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
            [input.project_id.as_str()],
            |row| row.get(0),
        )?;
        if !project_exists {
            return Err(StoreError::ProjectNotFound);
        }

        let prior_delivery: Option<(String, String)> = transaction
            .query_row(
                "SELECT id, request_digest
                 FROM input_envelopes
                 WHERE source_kind = ?1 AND delivery_id = ?2",
                params![input.source_kind, input.delivery_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((envelope_id, stored_request_digest)) = prior_delivery {
            if stored_request_digest != request_digest {
                return Err(StoreError::InputDeliveryConflict);
            }
            let envelope_id = parse_id(envelope_id, 0)?;
            let envelope = load_envelope(&transaction, &input.project_id, &envelope_id)?
                .ok_or(StoreError::InputEnvelopeNotFound)?;
            let candidate =
                load_candidate(&transaction, &input.project_id, &envelope.candidate_id)?
                    .ok_or(StoreError::WorkCandidateNotFound)?;
            transaction.commit()?;
            return Ok((
                InputReceipt {
                    envelope,
                    candidate,
                    replayed: true,
                },
                Vec::new(),
            ));
        }

        let existing_candidate: Option<(WorkCandidateId, String)> = transaction
            .query_row(
                "SELECT id, content_digest
                 FROM work_candidates
                 WHERE project_id = ?1 AND source_kind = ?2
                   AND source_id = ?3 AND source_revision = ?4",
                params![
                    input.project_id.as_str(),
                    input.source_kind,
                    input.source_id,
                    input.source_revision
                ],
                |row| Ok((parse_id(row.get(0)?, 0)?, row.get(1)?)),
            )
            .optional()?;

        let envelope_id = generated_id()?;
        if let Some((candidate_id, stored_content_digest)) = existing_candidate {
            if stored_content_digest != content_digest {
                return Err(StoreError::SourceRevisionConflict);
            }
            insert_envelope(
                &transaction,
                &envelope_id,
                &candidate_id,
                &input,
                &content_digest,
                &request_digest,
                now_ms,
            )?;
            let event = input_received_event(
                &transaction,
                &input.project_id,
                &envelope_id,
                &candidate_id,
                now_ms,
            )?;
            let envelope = load_envelope(&transaction, &input.project_id, &envelope_id)?
                .ok_or(StoreError::InputEnvelopeNotFound)?;
            let candidate = load_candidate(&transaction, &input.project_id, &candidate_id)?
                .ok_or(StoreError::WorkCandidateNotFound)?;
            transaction.commit()?;
            return Ok((
                InputReceipt {
                    envelope,
                    candidate,
                    replayed: false,
                },
                vec![event],
            ));
        }

        let current_candidate_id: Option<WorkCandidateId> = transaction
            .query_row(
                "SELECT current_candidate_id
                 FROM input_sources
                 WHERE project_id = ?1 AND source_kind = ?2 AND source_id = ?3",
                params![
                    input.project_id.as_str(),
                    input.source_kind,
                    input.source_id
                ],
                |row| parse_id(row.get(0)?, 0),
            )
            .optional()?;
        if current_candidate_id.as_ref() != input.expected_current_candidate_id.as_ref() {
            return Err(StoreError::StaleInputSource);
        }

        let candidate_id: WorkCandidateId = generated_id()?;
        insert_envelope_row(
            &transaction,
            &envelope_id,
            &input,
            &content_digest,
            &request_digest,
            now_ms,
        )?;
        transaction.execute(
            "INSERT INTO work_candidates (
                id, project_id, origin_envelope_id, source_kind, source_id,
                source_revision, content_digest, status, status_reason,
                revision, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'quarantined', NULL, 1, ?8, ?8)",
            params![
                candidate_id.as_str(),
                input.project_id.as_str(),
                envelope_id.as_str(),
                input.source_kind,
                input.source_id,
                input.source_revision,
                content_digest,
                now_ms
            ],
        )?;
        transaction.execute(
            "INSERT INTO work_candidate_envelopes (envelope_id, candidate_id, project_id)
             VALUES (?1, ?2, ?3)",
            params![
                envelope_id.as_str(),
                candidate_id.as_str(),
                input.project_id.as_str()
            ],
        )?;

        let mut events = Vec::new();
        if let Some(current_candidate_id) = current_candidate_id {
            let current_candidate =
                load_candidate(&transaction, &input.project_id, &current_candidate_id)?
                    .ok_or(StoreError::WorkCandidateNotFound)?;
            if current_candidate.status == WorkCandidateStatus::Stale {
                return Err(StoreError::InvalidWorkCandidateState);
            }
            let changed = transaction.execute(
                "UPDATE work_candidates
                 SET status = 'stale', status_reason = ?1,
                     revision = revision + 1, updated_at_ms = ?2
                 WHERE id = ?3 AND project_id = ?4 AND status = 'quarantined'",
                params![
                    STALE_REASON,
                    now_ms,
                    current_candidate_id.as_str(),
                    input.project_id.as_str()
                ],
            )?;
            if changed == 1 {
                events.push(candidate_status_event(
                    &transaction,
                    &input.project_id,
                    &current_candidate_id,
                    WorkCandidateStatus::Stale,
                    now_ms,
                )?);
            }
            transaction.execute(
                "UPDATE input_sources
                 SET current_candidate_id = ?1, revision = revision + 1
                 WHERE project_id = ?2 AND source_kind = ?3 AND source_id = ?4",
                params![
                    candidate_id.as_str(),
                    input.project_id.as_str(),
                    input.source_kind,
                    input.source_id
                ],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO input_sources (
                    project_id, source_kind, source_id, current_candidate_id, revision
                 ) VALUES (?1, ?2, ?3, ?4, 1)",
                params![
                    input.project_id.as_str(),
                    input.source_kind,
                    input.source_id,
                    candidate_id.as_str()
                ],
            )?;
        }
        events.push(input_received_event(
            &transaction,
            &input.project_id,
            &envelope_id,
            &candidate_id,
            now_ms,
        )?);

        let envelope = load_envelope(&transaction, &input.project_id, &envelope_id)?
            .ok_or(StoreError::InputEnvelopeNotFound)?;
        let candidate = load_candidate(&transaction, &input.project_id, &candidate_id)?
            .ok_or(StoreError::WorkCandidateNotFound)?;
        transaction.commit()?;
        Ok((
            InputReceipt {
                envelope,
                candidate,
                replayed: false,
            },
            events,
        ))
    }

    pub fn get_input_envelope(
        &self,
        project_id: &ProjectId,
        envelope_id: &InputEnvelopeId,
    ) -> Result<InputEnvelopeSnapshot> {
        load_envelope(&self.connection, project_id, envelope_id)?
            .ok_or(StoreError::InputEnvelopeNotFound)
    }

    pub fn list_input_envelopes(
        &self,
        project_id: &ProjectId,
        after_id: Option<&InputEnvelopeId>,
        limit: usize,
    ) -> Result<Vec<InputEnvelopeSnapshot>> {
        validate_page_limit(limit)?;
        let after_id = after_id.map_or("", InputEnvelopeId::as_str);
        let mut statement = self.connection.prepare(
            "SELECT e.id, e.project_id, m.candidate_id, e.source_kind, e.source_id,
                    e.delivery_id, e.source_revision, e.content, e.content_digest,
                    e.request_digest, e.received_at_ms
             FROM input_envelopes e
             JOIN work_candidate_envelopes m
               ON m.envelope_id = e.id AND m.project_id = e.project_id
             WHERE e.project_id = ?1 AND e.id > ?2
             ORDER BY e.id
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![project_id.as_str(), after_id, limit as i64],
            envelope_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_work_candidate(
        &self,
        project_id: &ProjectId,
        candidate_id: &WorkCandidateId,
    ) -> Result<WorkCandidateSnapshot> {
        load_candidate(&self.connection, project_id, candidate_id)?
            .ok_or(StoreError::WorkCandidateNotFound)
    }

    pub fn list_work_candidates(
        &self,
        project_id: &ProjectId,
        after_id: Option<&WorkCandidateId>,
        limit: usize,
    ) -> Result<Vec<WorkCandidateSnapshot>> {
        validate_page_limit(limit)?;
        let after_id = after_id.map_or("", WorkCandidateId::as_str);
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, origin_envelope_id, source_kind, source_id,
                    source_revision, content_digest, status, status_reason, revision,
                    created_at_ms, updated_at_ms
             FROM work_candidates
             WHERE project_id = ?1 AND id > ?2
             ORDER BY id
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![project_id.as_str(), after_id, limit as i64],
            candidate_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn reject_work_candidate(
        &mut self,
        project_id: &ProjectId,
        candidate_id: &WorkCandidateId,
        expected_revision: i64,
        reason: String,
        now_ms: i64,
    ) -> Result<(WorkCandidateSnapshot, Vec<EventEnvelope>)> {
        let reason = reason.trim().to_owned();
        if reason.is_empty()
            || reason.len() > MAX_STATUS_REASON_BYTES
            || expected_revision < 1
            || now_ms < 0
        {
            return Err(StoreError::InvalidWorkCandidateReason);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate = load_candidate(&transaction, project_id, candidate_id)?
            .ok_or(StoreError::WorkCandidateNotFound)?;
        let current_candidate_id: Option<WorkCandidateId> = transaction
            .query_row(
                "SELECT current_candidate_id
                 FROM input_sources
                 WHERE project_id = ?1 AND source_kind = ?2 AND source_id = ?3",
                params![
                    project_id.as_str(),
                    candidate.source_kind,
                    candidate.source_id
                ],
                |row| parse_id(row.get(0)?, 0),
            )
            .optional()?;
        if current_candidate_id.as_ref() != Some(candidate_id) {
            return Err(StoreError::InvalidWorkCandidateState);
        }

        let replay_revision = expected_revision.checked_add(1);
        if candidate.status == WorkCandidateStatus::Rejected
            && candidate.status_reason.as_deref() == Some(reason.as_str())
            && replay_revision == Some(candidate.revision)
        {
            transaction.commit()?;
            return Ok((candidate, Vec::new()));
        }
        if candidate.revision != expected_revision {
            return Err(StoreError::WorkCandidateRevisionConflict);
        }
        if candidate.status != WorkCandidateStatus::Quarantined {
            return Err(StoreError::InvalidWorkCandidateState);
        }
        transaction.execute(
            "UPDATE work_candidates
             SET status = 'rejected', status_reason = ?1,
                 revision = revision + 1, updated_at_ms = ?2
             WHERE id = ?3 AND project_id = ?4",
            params![reason, now_ms, candidate_id.as_str(), project_id.as_str()],
        )?;
        let event = candidate_status_event(
            &transaction,
            project_id,
            candidate_id,
            WorkCandidateStatus::Rejected,
            now_ms,
        )?;
        let candidate = load_candidate(&transaction, project_id, candidate_id)?
            .ok_or(StoreError::WorkCandidateNotFound)?;
        transaction.commit()?;
        Ok((candidate, vec![event]))
    }
}

fn validate_input(input: &NewInputEnvelope, now_ms: i64) -> Result<()> {
    let valid_source_kind = !input.source_kind.is_empty()
        && input.source_kind.len() <= MAX_SOURCE_KIND_BYTES
        && input.source_kind.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if !valid_source_kind
        || !bounded_nonempty(&input.source_id, MAX_SOURCE_ID_BYTES)
        || !bounded_nonempty(&input.delivery_id, MAX_DELIVERY_ID_BYTES)
        || !bounded_nonempty(&input.source_revision, MAX_SOURCE_REVISION_BYTES)
        || input.content.len() > MAX_INPUT_CONTENT_BYTES
        || now_ms < 0
    {
        return Err(StoreError::InvalidInputEnvelope);
    }
    Ok(())
}

fn bounded_nonempty(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}

fn validate_page_limit(limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_STATE_PAGE {
        return Err(StoreError::InvalidStateLimit);
    }
    Ok(())
}

fn generated_id<T>() -> Result<T>
where
    T: TryFrom<String>,
{
    T::try_from(Uuid::new_v4().to_string()).map_err(|_| StoreError::InvalidInputEnvelope)
}

fn insert_envelope(
    connection: &Connection,
    envelope_id: &InputEnvelopeId,
    candidate_id: &WorkCandidateId,
    input: &NewInputEnvelope,
    content_digest: &str,
    request_digest: &str,
    now_ms: i64,
) -> Result<()> {
    insert_envelope_row(
        connection,
        envelope_id,
        input,
        content_digest,
        request_digest,
        now_ms,
    )?;
    connection.execute(
        "INSERT INTO work_candidate_envelopes (envelope_id, candidate_id, project_id)
         VALUES (?1, ?2, ?3)",
        params![
            envelope_id.as_str(),
            candidate_id.as_str(),
            input.project_id.as_str()
        ],
    )?;
    Ok(())
}

fn insert_envelope_row(
    connection: &Connection,
    envelope_id: &InputEnvelopeId,
    input: &NewInputEnvelope,
    content_digest: &str,
    request_digest: &str,
    now_ms: i64,
) -> Result<()> {
    connection.execute(
        "INSERT INTO input_envelopes (
            id, project_id, source_kind, source_id, delivery_id,
            source_revision, content, content_digest, request_digest, received_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            envelope_id.as_str(),
            input.project_id.as_str(),
            input.source_kind,
            input.source_id,
            input.delivery_id,
            input.source_revision,
            input.content,
            content_digest,
            request_digest,
            now_ms
        ],
    )?;
    Ok(())
}

fn input_received_event(
    connection: &Transaction<'_>,
    project_id: &ProjectId,
    envelope_id: &InputEnvelopeId,
    candidate_id: &WorkCandidateId,
    now_ms: i64,
) -> Result<EventEnvelope> {
    let event = FactoryEvent::InputReceived {
        project_id: project_id.clone(),
        envelope_id: envelope_id.clone(),
        candidate_id: candidate_id.clone(),
    };
    let sequence = append_event(connection, now_ms, &event)?;
    Ok(EventEnvelope {
        protocol_version: PROTOCOL_VERSION,
        sequence,
        occurred_at_ms: now_ms,
        event,
    })
}

fn candidate_status_event(
    connection: &Transaction<'_>,
    project_id: &ProjectId,
    candidate_id: &WorkCandidateId,
    status: WorkCandidateStatus,
    now_ms: i64,
) -> Result<EventEnvelope> {
    let event = FactoryEvent::WorkCandidateStatusChanged {
        project_id: project_id.clone(),
        candidate_id: candidate_id.clone(),
        status,
    };
    let sequence = append_event(connection, now_ms, &event)?;
    Ok(EventEnvelope {
        protocol_version: PROTOCOL_VERSION,
        sequence,
        occurred_at_ms: now_ms,
        event,
    })
}

fn load_envelope(
    connection: &Connection,
    project_id: &ProjectId,
    envelope_id: &InputEnvelopeId,
) -> Result<Option<InputEnvelopeSnapshot>> {
    connection
        .query_row(
            "SELECT e.id, e.project_id, m.candidate_id, e.source_kind, e.source_id,
                    e.delivery_id, e.source_revision, e.content, e.content_digest,
                    e.request_digest, e.received_at_ms
             FROM input_envelopes e
             JOIN work_candidate_envelopes m
               ON m.envelope_id = e.id AND m.project_id = e.project_id
             WHERE e.project_id = ?1 AND e.id = ?2",
            params![project_id.as_str(), envelope_id.as_str()],
            envelope_from_row,
        )
        .optional()
        .map_err(StoreError::from)
}

fn envelope_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InputEnvelopeSnapshot> {
    Ok(InputEnvelopeSnapshot {
        id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        candidate_id: parse_id(row.get(2)?, 2)?,
        source_kind: row.get(3)?,
        source_id: row.get(4)?,
        delivery_id: row.get(5)?,
        source_revision: row.get(6)?,
        content: row.get(7)?,
        content_digest: row.get(8)?,
        request_digest: row.get(9)?,
        received_at_ms: row.get(10)?,
    })
}

fn load_candidate(
    connection: &Connection,
    project_id: &ProjectId,
    candidate_id: &WorkCandidateId,
) -> Result<Option<WorkCandidateSnapshot>> {
    connection
        .query_row(
            "SELECT id, project_id, origin_envelope_id, source_kind, source_id,
                    source_revision, content_digest, status, status_reason, revision,
                    created_at_ms, updated_at_ms
             FROM work_candidates
             WHERE project_id = ?1 AND id = ?2",
            params![project_id.as_str(), candidate_id.as_str()],
            candidate_from_row,
        )
        .optional()
        .map_err(StoreError::from)
}

fn candidate_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkCandidateSnapshot> {
    let status: String = row.get(7)?;
    Ok(WorkCandidateSnapshot {
        id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        origin_envelope_id: parse_id(row.get(2)?, 2)?,
        source_kind: row.get(3)?,
        source_id: row.get(4)?,
        source_revision: row.get(5)?,
        content_digest: row.get(6)?,
        status: parse_status(&status, 7)?,
        status_reason: row.get(8)?,
        revision: row.get(9)?,
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
    })
}

fn parse_status(value: &str, column: usize) -> rusqlite::Result<WorkCandidateStatus> {
    let status = match value {
        "quarantined" => WorkCandidateStatus::Quarantined,
        "stale" => WorkCandidateStatus::Stale,
        "rejected" => WorkCandidateStatus::Rejected,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                column,
                Type::Text,
                format!("invalid work candidate status {value:?}").into(),
            ));
        }
    };
    Ok(status)
}

fn parse_id<T>(value: String, column: usize) -> rusqlite::Result<T>
where
    T: TryFrom<String>,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    T::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn request_digest(input: &NewInputEnvelope, content_digest: &str) -> String {
    let mut digest = Sha256::new();
    digest_field(&mut digest, b"dark-factory-input-receipt-v1");
    digest_field(&mut digest, input.project_id.as_str().as_bytes());
    digest_field(&mut digest, input.source_kind.as_bytes());
    digest_field(&mut digest, input.source_id.as_bytes());
    digest_field(&mut digest, input.delivery_id.as_bytes());
    digest_field(&mut digest, input.source_revision.as_bytes());
    digest_field(&mut digest, content_digest.as_bytes());
    match &input.expected_current_candidate_id {
        Some(candidate_id) => {
            digest_field(&mut digest, b"some");
            digest_field(&mut digest, candidate_id.as_str().as_bytes());
        }
        None => digest_field(&mut digest, b"none"),
    }
    format!("{:x}", digest.finalize())
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewProject;

    fn fixture(store: &mut Store) -> ProjectId {
        let project_id = ProjectId::try_from("project-1").unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Project".into(),
                    root: "/tmp/project".into(),
                },
                1,
            )
            .unwrap();
        project_id
    }

    fn input(project_id: &ProjectId, delivery: &str, revision: &str) -> NewInputEnvelope {
        NewInputEnvelope {
            project_id: project_id.clone(),
            source_kind: "fixture".into(),
            source_id: "source-1".into(),
            delivery_id: delivery.into(),
            source_revision: revision.into(),
            content: format!("body-{revision}"),
            expected_current_candidate_id: None,
        }
    }

    #[test]
    fn exact_delivery_replay_returns_the_same_quarantine_identity_without_an_event() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = fixture(&mut store);
        let request = input(&project_id, "delivery-1", "revision-1");
        let (first, first_events) = store.receive_input(request.clone(), 2).unwrap();
        let (replay, replay_events) = store.receive_input(request, 3).unwrap();

        assert!(!first.replayed);
        assert_eq!(first_events.len(), 1);
        assert!(replay.replayed);
        assert!(replay_events.is_empty());
        assert_eq!(replay.envelope.id, first.envelope.id);
        assert_eq!(replay.candidate.id, first.candidate.id);
        let event_json = serde_json::to_string(&first_events).unwrap();
        assert!(!event_json.contains("body-revision-1"));
        assert!(!event_json.contains("source-1"));
    }

    #[test]
    fn changed_delivery_and_changed_source_revision_bytes_fail_closed() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = fixture(&mut store);
        let request = input(&project_id, "delivery-1", "revision-1");
        store.receive_input(request.clone(), 2).unwrap();

        let mut changed_delivery = request.clone();
        changed_delivery.content = "different".into();
        assert!(matches!(
            store.receive_input(changed_delivery, 3),
            Err(StoreError::InputDeliveryConflict)
        ));

        let mut changed_revision = request;
        changed_revision.delivery_id = "delivery-2".into();
        changed_revision.content = "different".into();
        assert!(matches!(
            store.receive_input(changed_revision, 3),
            Err(StoreError::SourceRevisionConflict)
        ));
    }

    #[test]
    fn different_unordered_source_revision_requires_exact_current_and_stales_atomically() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = fixture(&mut store);
        let (first, _) = store
            .receive_input(input(&project_id, "delivery-1", "z-revision"), 2)
            .unwrap();
        let second = input(&project_id, "delivery-2", "a-revision");
        assert!(matches!(
            store.receive_input(second.clone(), 3),
            Err(StoreError::StaleInputSource)
        ));

        let mut second = second;
        second.expected_current_candidate_id = Some(first.candidate.id.clone());
        let (second, events) = store.receive_input(second, 3).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(second.candidate.status, WorkCandidateStatus::Quarantined);
        let first = store
            .get_work_candidate(&project_id, &first.candidate.id)
            .unwrap();
        assert_eq!(first.status, WorkCandidateStatus::Stale);
        assert_eq!(first.status_reason.as_deref(), Some(STALE_REASON));
        assert_eq!(first.revision, 2);
    }

    #[test]
    fn reconciliation_observation_converges_on_one_candidate_revision() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = fixture(&mut store);
        let (first, _) = store
            .receive_input(input(&project_id, "webhook-1", "revision-1"), 2)
            .unwrap();
        let (reconciled, events) = store
            .receive_input(input(&project_id, "reconcile-1", "revision-1"), 3)
            .unwrap();

        assert_ne!(reconciled.envelope.id, first.envelope.id);
        assert_eq!(reconciled.candidate.id, first.candidate.id);
        assert_eq!(events.len(), 1);
        assert_eq!(
            store
                .list_work_candidates(&project_id, None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .list_input_envelopes(&project_id, None, 10)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn rejection_is_revision_bound_idempotent_and_cannot_materialize_work() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = fixture(&mut store);
        let (receipt, _) = store
            .receive_input(input(&project_id, "delivery-1", "revision-1"), 2)
            .unwrap();
        let (rejected, events) = store
            .reject_work_candidate(
                &project_id,
                &receipt.candidate.id,
                1,
                "not approved".into(),
                3,
            )
            .unwrap();
        assert_eq!(rejected.status, WorkCandidateStatus::Rejected);
        assert_eq!(rejected.revision, 2);
        assert_eq!(events.len(), 1);
        let (retried, retry_events) = store
            .reject_work_candidate(
                &project_id,
                &receipt.candidate.id,
                1,
                "not approved".into(),
                4,
            )
            .unwrap();
        assert_eq!(retried, rejected);
        assert!(retry_events.is_empty());

        for table in ["tasks", "agent_messages", "runs", "changes"] {
            let count: i64 = store
                .connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?1"),
                    [project_id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "receipt unexpectedly created a {table} row");
        }
    }

    #[test]
    fn quarantine_survives_store_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let (project_id, envelope_id, candidate_id) = {
            let mut store = Store::open(file.path()).unwrap();
            let project_id = fixture(&mut store);
            let (receipt, _) = store
                .receive_input(input(&project_id, "delivery-1", "revision-1"), 2)
                .unwrap();
            (project_id, receipt.envelope.id, receipt.candidate.id)
        };
        let store = Store::open(file.path()).unwrap();
        assert_eq!(
            store
                .get_input_envelope(&project_id, &envelope_id)
                .unwrap()
                .candidate_id,
            candidate_id
        );
        assert_eq!(
            store
                .get_work_candidate(&project_id, &candidate_id)
                .unwrap()
                .status,
            WorkCandidateStatus::Quarantined
        );
    }

    #[test]
    fn deleting_a_project_removes_its_private_quarantine_rows() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = fixture(&mut store);
        let (receipt, _) = store
            .receive_input(input(&project_id, "delivery-1", "revision-1"), 2)
            .unwrap();

        store.delete_project(&project_id, 3).unwrap();

        assert!(matches!(
            store.get_input_envelope(&project_id, &receipt.envelope.id),
            Err(StoreError::InputEnvelopeNotFound)
        ));
        assert!(matches!(
            store.get_work_candidate(&project_id, &receipt.candidate.id),
            Err(StoreError::WorkCandidateNotFound)
        ));
    }
}
