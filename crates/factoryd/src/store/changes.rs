use std::path::Path;

use factory_core::{
    AgentId, ChangeId, ChangePhase, ChangeSnapshot, EventEnvelope, FactoryEvent, LegacySourceId,
    LegacySourceSnapshot, PROTOCOL_VERSION, ProjectId, TaskId,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params, types::Type};

use super::{MAX_PATH_BYTES, Result, Store, StoreError, append_event, truncate_utf8};

const MAX_TASK_INCARNATION_BYTES: usize = 255;
const MAX_CHANGE_FAILURE_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Change {
    pub id: ChangeId,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub task_incarnation_id: String,
    pub phase: ChangePhase,
    pub source_root: String,
    pub base_oid: Option<String>,
    pub base_repository_root: Option<String>,
    pub base_repository_dev: Option<u64>,
    pub base_repository_inode: Option<u64>,
    pub revision: i64,
    pub source_dev: Option<u64>,
    pub source_inode: Option<u64>,
    pub size_bytes: Option<u64>,
    pub measured_at_ms: Option<i64>,
    pub last_failure: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub available_at_ms: Option<i64>,
    pub removing_at_ms: Option<i64>,
    pub removed_at_ms: Option<i64>,
}

pub struct ChangeReservation {
    pub id: ChangeId,
    pub source_root: String,
    pub max_factory_changes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSourceIdentity {
    pub source_root: String,
    pub device: u64,
    pub inode: u64,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeBaseIdentity {
    pub repository_root: String,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeMaterialization {
    pub base_oid: String,
    pub base: ChangeBaseIdentity,
    pub source: ChangeSourceIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeStorageSummary {
    pub retained_count: u64,
    pub measured_bytes: u64,
    pub measured_at_ms: Option<i64>,
    pub active_leases: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChangeRemovalKind {
    Provisioning,
    Published(ChangeSourceIdentity),
}

/// One atomic durable Change mutation and its matching event projection.
pub struct ChangeMutation {
    pub change: Change,
    pub event: Option<EventEnvelope>,
}

impl ChangeMutation {
    #[must_use]
    pub fn into_parts(self) -> (Change, Vec<EventEnvelope>) {
        (self.change, self.event.into_iter().collect())
    }
}

impl Change {
    #[must_use]
    pub fn snapshot(&self) -> ChangeSnapshot {
        ChangeSnapshot {
            id: self.id.clone(),
            project_id: self.project_id.clone(),
            task_id: self.task_id.clone(),
            task_incarnation_id: self.task_incarnation_id.clone(),
            phase: self.phase,
            base_oid: self.base_oid.clone(),
            revision: self.revision,
            measured_bytes: self.size_bytes,
            measured_at_ms: self.measured_at_ms,
            failure: self.last_failure.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            available_at_ms: self.available_at_ms,
            removed_at_ms: self.removed_at_ms,
        }
    }

    pub fn removal_kind(&self) -> Result<ChangeRemovalKind> {
        match (self.available_at_ms, self.source_dev, self.source_inode) {
            (None, None, None) => Ok(ChangeRemovalKind::Provisioning),
            (Some(_), Some(device), Some(inode)) => {
                Ok(ChangeRemovalKind::Published(ChangeSourceIdentity {
                    source_root: self.source_root.clone(),
                    device,
                    inode,
                    size_bytes: self.size_bytes.unwrap_or(0),
                }))
            }
            _ => Err(StoreError::InvalidChangeMetadata),
        }
    }
}

impl Store {
    pub fn change(&self, project_id: &ProjectId, change_id: &ChangeId) -> Result<Option<Change>> {
        load_change(&self.connection, project_id, change_id)
    }

    pub fn recoverable_changes(&self) -> Result<Vec<Change>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, task_id, task_incarnation_id, phase, source_root,
                    base_oid, base_repository_root, base_repository_dev,
                    base_repository_inode, revision, source_dev, source_inode, size_bytes,
                    measured_at_ms, last_failure,
                    created_at_ms, updated_at_ms, available_at_ms, removing_at_ms,
                    removed_at_ms
             FROM changes
             WHERE phase = 'removing'
                OR (phase IN ('provisioning', 'available')
                    AND size_bytes IS NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM runs
                        WHERE runs.change_id = changes.id
                          AND runs.project_id = changes.project_id
                          AND runs.phase <> 'terminal'
                    ))
             ORDER BY updated_at_ms, id",
        )?;
        statement
            .query_map([], change_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_changes(
        &self,
        project_id: &ProjectId,
        after_id: Option<&ChangeId>,
        limit: usize,
    ) -> Result<Vec<ChangeSnapshot>> {
        if !(1..=super::MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, task_id, task_incarnation_id, phase, source_root,
                    base_oid, base_repository_root, base_repository_dev,
                    base_repository_inode, revision, source_dev, source_inode, size_bytes,
                    measured_at_ms, last_failure,
                    created_at_ms, updated_at_ms, available_at_ms, removing_at_ms,
                    removed_at_ms
             FROM changes
             WHERE project_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id LIMIT ?3",
        )?;
        statement
            .query_map(
                params![
                    project_id.as_str(),
                    after_id.map(ChangeId::as_str),
                    i64::try_from(limit).map_err(|_| StoreError::InvalidStateLimit)?,
                ],
                change_from_row,
            )?
            .map(|row| row.map(|change| change.snapshot()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn change_storage_summary(
        &self,
        project_id: Option<&ProjectId>,
    ) -> Result<ChangeStorageSummary> {
        let (count, bytes, missing, unstable, measured_at): (i64, i64, i64, i64, Option<i64>) =
            self.connection.query_row(
                "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0),
                        COUNT(*) FILTER (WHERE size_bytes IS NULL),
                        COUNT(*) FILTER (WHERE phase = 'removing'),
                        MIN(measured_at_ms)
                 FROM changes
                 WHERE (?1 IS NULL OR project_id = ?1) AND phase <> 'removed'",
                [project_id.map(ProjectId::as_str)],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        let active_leases: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM runs
             WHERE (?1 IS NULL OR project_id = ?1)
               AND change_id IS NOT NULL AND phase <> 'terminal'",
            [project_id.map(ProjectId::as_str)],
            |row| row.get(0),
        )?;
        Ok(ChangeStorageSummary {
            retained_count: u64::try_from(count).map_err(|_| StoreError::InvalidChangeMetadata)?,
            measured_bytes: u64::try_from(bytes).map_err(|_| StoreError::InvalidChangeMetadata)?,
            measured_at_ms: measured_at,
            active_leases: u64::try_from(active_leases)
                .map_err(|_| StoreError::InvalidChangeMetadata)?,
            complete: missing == 0 && unstable == 0 && active_leases == 0,
        })
    }

    pub fn list_legacy_sources(
        &self,
        project_id: &ProjectId,
        after_id: Option<&LegacySourceId>,
        limit: usize,
    ) -> Result<Vec<LegacySourceSnapshot>> {
        if !(1..=super::MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, former_agent_id, source_path,
                    retained_reason, recorded_at_ms
             FROM legacy_sources
             WHERE project_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id LIMIT ?3",
        )?;
        statement
            .query_map(
                params![
                    project_id.as_str(),
                    after_id.map(LegacySourceId::as_str),
                    i64::try_from(limit).map_err(|_| StoreError::InvalidStateLimit)?,
                ],
                |row| {
                    let former_agent_id = row.get::<_, Option<String>>(2)?;
                    Ok(LegacySourceSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        former_agent_id: former_agent_id
                            .map(|value| parse_id::<AgentId>(value, 2))
                            .transpose()?,
                        source_path: row.get(3)?,
                        retained_reason: row.get(4)?,
                        recorded_at_ms: row.get(5)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn forget_legacy_source(
        &mut self,
        project_id: &ProjectId,
        legacy_source_id: &LegacySourceId,
        now_ms: i64,
    ) -> Result<EventEnvelope> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute(
            "DELETE FROM legacy_sources WHERE id = ?1 AND project_id = ?2",
            params![legacy_source_id.as_str(), project_id.as_str()],
        )?;
        if deleted != 1 {
            return Err(StoreError::LegacySourceNotFound);
        }
        let event = FactoryEvent::LegacySourceForgotten {
            project_id: project_id.clone(),
            legacy_source_id: legacy_source_id.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        })
    }

    pub fn record_change_base(
        &mut self,
        project_id: &ProjectId,
        change_id: &ChangeId,
        expected_revision: i64,
        base_oid: &str,
        identity: &ChangeBaseIdentity,
        now_ms: i64,
    ) -> Result<ChangeMutation> {
        validate_revision(expected_revision)?;
        if !valid_oid(base_oid) || !valid_base_identity(identity) {
            return Err(StoreError::InvalidChangeMetadata);
        }
        let device = sqlite_u64(identity.device)?;
        let inode = sqlite_positive_u64(identity.inode)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE changes
             SET base_oid = ?1, base_repository_root = ?2,
                 base_repository_dev = ?3, base_repository_inode = ?4,
                 revision = revision + 1, last_failure = NULL, updated_at_ms = ?5
             WHERE id = ?6 AND project_id = ?7 AND phase = 'provisioning'
               AND revision = ?8 AND base_oid IS NULL",
            params![
                base_oid,
                identity.repository_root,
                device,
                inode,
                now_ms,
                change_id.as_str(),
                project_id.as_str(),
                expected_revision,
            ],
        )?;
        if changed == 0 {
            let current = load_change(&transaction, project_id, change_id)?
                .ok_or(StoreError::ChangeNotFound)?;
            if current.phase == ChangePhase::Provisioning
                && current.base_oid.as_deref() == Some(base_oid)
                && current.base_repository_root.as_deref()
                    == Some(identity.repository_root.as_str())
                && current.base_repository_dev == Some(identity.device)
                && current.base_repository_inode == Some(identity.inode)
                && (current.revision == expected_revision
                    || expected_revision
                        .checked_add(1)
                        .is_some_and(|value| current.revision == value))
            {
                transaction.commit()?;
                return Ok(ChangeMutation {
                    change: current,
                    event: None,
                });
            }
        }
        ensure_change_transition(
            &transaction,
            project_id,
            change_id,
            expected_revision,
            ChangePhase::Provisioning,
            changed,
        )?;
        let change =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        let mutation = change_mutation(&transaction, change, now_ms)?;
        transaction.commit()?;
        Ok(mutation)
    }

    pub fn mark_change_available(
        &mut self,
        project_id: &ProjectId,
        change_id: &ChangeId,
        expected_revision: i64,
        materialization: &ChangeMaterialization,
        now_ms: i64,
    ) -> Result<ChangeMutation> {
        let ChangeMaterialization {
            base_oid,
            base,
            source,
        } = materialization;
        if !valid_oid(base_oid) || !valid_base_identity(base) {
            return Err(StoreError::InvalidChangeMetadata);
        }
        validate_source_identity(source)?;
        validate_revision(expected_revision)?;
        let device = sqlite_u64(source.device)?;
        let inode = sqlite_positive_u64(source.inode)?;
        let size = sqlite_u64(source.size_bytes)?;
        let base_device = sqlite_u64(base.device)?;
        let base_inode = sqlite_positive_u64(base.inode)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE changes
             SET phase = 'available', revision = revision + 1,
                 source_dev = ?1, source_inode = ?2, size_bytes = ?3,
                 measured_at_ms = ?4, available_at_ms = ?4,
                 last_failure = NULL, updated_at_ms = ?4
             WHERE id = ?5 AND project_id = ?6 AND phase = 'provisioning'
               AND revision = ?7 AND source_root = ?8 AND base_oid = ?9
               AND base_repository_root = ?10 AND base_repository_dev = ?11
               AND base_repository_inode = ?12",
            params![
                device,
                inode,
                size,
                now_ms,
                change_id.as_str(),
                project_id.as_str(),
                expected_revision,
                source.source_root,
                base_oid,
                base.repository_root,
                base_device,
                base_inode,
            ],
        )?;
        ensure_change_transition(
            &transaction,
            project_id,
            change_id,
            expected_revision,
            ChangePhase::Provisioning,
            changed,
        )?;
        let change =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        let mutation = change_mutation(&transaction, change, now_ms)?;
        transaction.commit()?;
        Ok(mutation)
    }

    pub fn begin_change_removal(
        &mut self,
        project_id: &ProjectId,
        change_id: &ChangeId,
        expected_revision: i64,
        now_ms: i64,
    ) -> Result<ChangeMutation> {
        validate_revision(expected_revision)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let leased: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM runs
                 WHERE change_id = ?1 AND project_id = ?2 AND phase <> 'terminal'
             )",
            params![change_id.as_str(), project_id.as_str()],
            |row| row.get(0),
        )?;
        if leased {
            return Err(StoreError::ChangeLeased);
        }
        let current =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        if current.revision != expected_revision {
            return Err(StoreError::ChangeRevisionConflict);
        }
        if !matches!(
            current.phase,
            ChangePhase::Provisioning | ChangePhase::Available
        ) {
            return Err(StoreError::InvalidChangeState);
        }
        current.removal_kind()?;
        let changed = transaction.execute(
            "UPDATE changes
             SET phase = 'removing', revision = revision + 1,
                 removing_at_ms = ?1, updated_at_ms = ?1
             WHERE id = ?2 AND project_id = ?3
               AND phase IN ('provisioning', 'available') AND revision = ?4",
            params![
                now_ms,
                change_id.as_str(),
                project_id.as_str(),
                expected_revision,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::ChangeRevisionConflict);
        }
        let change =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        let mutation = change_mutation(&transaction, change, now_ms)?;
        transaction.commit()?;
        Ok(mutation)
    }

    pub fn mark_change_removed(
        &mut self,
        project_id: &ProjectId,
        change_id: &ChangeId,
        expected_revision: i64,
        now_ms: i64,
    ) -> Result<ChangeMutation> {
        validate_revision(expected_revision)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        if current.revision != expected_revision {
            return Err(StoreError::ChangeRevisionConflict);
        }
        if current.phase != ChangePhase::Removing {
            return Err(StoreError::InvalidChangeState);
        }
        current.removal_kind()?;
        let changed = transaction.execute(
            "UPDATE changes
             SET phase = 'removed', revision = revision + 1,
                 removed_at_ms = ?1, last_failure = NULL, updated_at_ms = ?1
             WHERE id = ?2 AND project_id = ?3 AND phase = 'removing'
               AND revision = ?4",
            params![
                now_ms,
                change_id.as_str(),
                project_id.as_str(),
                expected_revision,
            ],
        )?;
        ensure_change_transition(
            &transaction,
            project_id,
            change_id,
            expected_revision,
            ChangePhase::Removing,
            changed,
        )?;
        let change =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        let mutation = change_mutation(&transaction, change, now_ms)?;
        transaction.commit()?;
        Ok(mutation)
    }

    pub fn record_change_measurement(
        &mut self,
        project_id: &ProjectId,
        change_id: &ChangeId,
        expected_revision: i64,
        identity: &ChangeSourceIdentity,
        now_ms: i64,
    ) -> Result<ChangeMutation> {
        validate_source_identity(identity)?;
        validate_revision(expected_revision)?;
        let device = sqlite_u64(identity.device)?;
        let inode = sqlite_positive_u64(identity.inode)?;
        let size = sqlite_u64(identity.size_bytes)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let leased: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM runs
                 WHERE change_id = ?1 AND project_id = ?2 AND phase <> 'terminal'
             )",
            params![change_id.as_str(), project_id.as_str()],
            |row| row.get(0),
        )?;
        if leased {
            return Err(StoreError::ChangeLeased);
        }
        let changed = transaction.execute(
            "UPDATE changes
             SET size_bytes = ?1, measured_at_ms = ?2,
                 revision = revision + 1, updated_at_ms = ?2
             WHERE id = ?3 AND project_id = ?4 AND phase = 'available'
               AND revision = ?5 AND source_root = ?6
               AND source_dev = ?7 AND source_inode = ?8",
            params![
                size,
                now_ms,
                change_id.as_str(),
                project_id.as_str(),
                expected_revision,
                identity.source_root,
                device,
                inode,
            ],
        )?;
        ensure_change_transition(
            &transaction,
            project_id,
            change_id,
            expected_revision,
            ChangePhase::Available,
            changed,
        )?;
        let change =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        let mutation = change_mutation(&transaction, change, now_ms)?;
        transaction.commit()?;
        Ok(mutation)
    }

    pub fn record_provisioning_measurement(
        &mut self,
        project_id: &ProjectId,
        change_id: &ChangeId,
        expected_revision: i64,
        size_bytes: u64,
        now_ms: i64,
    ) -> Result<ChangeMutation> {
        validate_revision(expected_revision)?;
        let size = sqlite_u64(size_bytes)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let leased: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM runs
                 WHERE change_id = ?1 AND project_id = ?2 AND phase <> 'terminal'
             )",
            params![change_id.as_str(), project_id.as_str()],
            |row| row.get(0),
        )?;
        if leased {
            return Err(StoreError::ChangeLeased);
        }
        let changed = transaction.execute(
            "UPDATE changes
             SET size_bytes = ?1, measured_at_ms = ?2,
                 revision = revision + 1, updated_at_ms = ?2
             WHERE id = ?3 AND project_id = ?4 AND phase = 'provisioning'
               AND revision = ?5",
            params![
                size,
                now_ms,
                change_id.as_str(),
                project_id.as_str(),
                expected_revision,
            ],
        )?;
        ensure_change_transition(
            &transaction,
            project_id,
            change_id,
            expected_revision,
            ChangePhase::Provisioning,
            changed,
        )?;
        let change =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        let mutation = change_mutation(&transaction, change, now_ms)?;
        transaction.commit()?;
        Ok(mutation)
    }

    pub fn record_change_failure(
        &mut self,
        project_id: &ProjectId,
        change_id: &ChangeId,
        expected_revision: i64,
        failure: &str,
        now_ms: i64,
    ) -> Result<ChangeMutation> {
        if failure.is_empty() {
            return Err(StoreError::InvalidChangeMetadata);
        }
        let failure = truncate_utf8(failure, MAX_CHANGE_FAILURE_BYTES);
        validate_revision(expected_revision)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        if current.revision != expected_revision {
            return Err(StoreError::ChangeRevisionConflict);
        }
        if current.last_failure.as_deref() == Some(failure.as_str())
            && matches!(
                current.phase,
                ChangePhase::Provisioning | ChangePhase::Removing
            )
        {
            transaction.commit()?;
            return Ok(ChangeMutation {
                change: current,
                event: None,
            });
        }
        let changed = transaction.execute(
            "UPDATE changes
             SET revision = revision + 1, last_failure = ?1, updated_at_ms = ?2
             WHERE id = ?3 AND project_id = ?4
               AND phase IN ('provisioning', 'removing')
               AND revision = ?5",
            params![
                failure,
                now_ms,
                change_id.as_str(),
                project_id.as_str(),
                expected_revision
            ],
        )?;
        if changed == 0 {
            let current = load_change(&transaction, project_id, change_id)?
                .ok_or(StoreError::ChangeNotFound)?;
            if current.revision != expected_revision {
                return Err(StoreError::ChangeRevisionConflict);
            }
            return Err(StoreError::InvalidChangeState);
        }
        let change =
            load_change(&transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
        let mutation = change_mutation(&transaction, change, now_ms)?;
        transaction.commit()?;
        Ok(mutation)
    }
}

pub(super) fn reserve_change(
    transaction: &rusqlite::Transaction<'_>,
    project_id: &ProjectId,
    task_id: &TaskId,
    task_incarnation_id: &str,
    reservation: &ChangeReservation,
    now_ms: i64,
) -> Result<ChangeMutation> {
    validate_reservation(reservation, task_incarnation_id)?;
    let retained: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM changes WHERE phase <> 'removed'",
        [],
        |row| row.get(0),
    )?;
    if retained >= i64::try_from(reservation.max_factory_changes).unwrap_or(i64::MAX) {
        return Err(StoreError::ChangeCapacityReached {
            limit: reservation.max_factory_changes,
        });
    }
    transaction.execute(
        "INSERT INTO changes (
            id, project_id, task_id, task_incarnation_id, phase, source_root,
            base_oid, base_repository_root, base_repository_dev,
            base_repository_inode, revision, source_dev, source_inode, size_bytes,
            measured_at_ms, last_failure,
            created_at_ms, updated_at_ms, available_at_ms, removing_at_ms,
            removed_at_ms
         ) VALUES (
            ?1, ?2, ?3, ?4, 'provisioning', ?5,
            NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL,
            ?6, ?6, NULL, NULL, NULL
         )",
        params![
            reservation.id.as_str(),
            project_id.as_str(),
            task_id.as_str(),
            task_incarnation_id,
            reservation.source_root,
            now_ms,
        ],
    )?;
    let change =
        load_change(transaction, project_id, &reservation.id)?.ok_or(StoreError::ChangeNotFound)?;
    change_mutation(transaction, change, now_ms)
}

pub(super) fn invalidate_change_measurement(
    transaction: &rusqlite::Transaction<'_>,
    project_id: &ProjectId,
    change_id: &ChangeId,
    now_ms: i64,
) -> Result<Option<EventEnvelope>> {
    let changed = transaction.execute(
        "UPDATE changes
         SET size_bytes = NULL, measured_at_ms = NULL,
             revision = revision + 1, updated_at_ms = ?1
         WHERE id = ?2 AND project_id = ?3
           AND phase IN ('provisioning', 'available')
           AND (size_bytes IS NOT NULL OR measured_at_ms IS NOT NULL)",
        params![now_ms, change_id.as_str(), project_id.as_str()],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let change =
        load_change(transaction, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
    Ok(change_mutation(transaction, change, now_ms)?.event)
}

fn ensure_change_transition(
    connection: &rusqlite::Connection,
    project_id: &ProjectId,
    change_id: &ChangeId,
    expected_revision: i64,
    expected_phase: ChangePhase,
    changed: usize,
) -> Result<()> {
    if changed == 1 {
        return Ok(());
    }
    let current =
        load_change(connection, project_id, change_id)?.ok_or(StoreError::ChangeNotFound)?;
    if current.revision != expected_revision {
        return Err(StoreError::ChangeRevisionConflict);
    }
    if current.phase != expected_phase {
        return Err(StoreError::InvalidChangeState);
    }
    Err(StoreError::ChangeIdentityMismatch)
}

pub(super) fn load_change(
    connection: &rusqlite::Connection,
    project_id: &ProjectId,
    change_id: &ChangeId,
) -> Result<Option<Change>> {
    connection
        .query_row(
            "SELECT id, project_id, task_id, task_incarnation_id, phase, source_root,
                    base_oid, base_repository_root, base_repository_dev,
                    base_repository_inode, revision, source_dev, source_inode, size_bytes,
                    measured_at_ms, last_failure,
                    created_at_ms, updated_at_ms, available_at_ms, removing_at_ms,
                    removed_at_ms
             FROM changes WHERE id = ?1 AND project_id = ?2",
            params![change_id.as_str(), project_id.as_str()],
            change_from_row,
        )
        .optional()
        .map_err(StoreError::from)
}

fn change_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    let phase: String = row.get(4)?;
    let base_repository_dev: Option<i64> = row.get(8)?;
    let base_repository_inode: Option<i64> = row.get(9)?;
    let source_dev: Option<i64> = row.get(11)?;
    let source_inode: Option<i64> = row.get(12)?;
    let size_bytes: Option<i64> = row.get(13)?;
    Ok(Change {
        id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        task_id: parse_id(row.get(2)?, 2)?,
        task_incarnation_id: row.get(3)?,
        phase: parse_change_phase(&phase).ok_or_else(|| invalid_value(4, phase))?,
        source_root: row.get(5)?,
        base_oid: row.get(6)?,
        base_repository_root: row.get(7)?,
        base_repository_dev: parse_optional_u64(base_repository_dev, 8)?,
        base_repository_inode: parse_optional_u64(base_repository_inode, 9)?,
        revision: row.get(10)?,
        source_dev: parse_optional_u64(source_dev, 11)?,
        source_inode: parse_optional_u64(source_inode, 12)?,
        size_bytes: parse_optional_u64(size_bytes, 13)?,
        measured_at_ms: row.get(14)?,
        last_failure: row.get(15)?,
        created_at_ms: row.get(16)?,
        updated_at_ms: row.get(17)?,
        available_at_ms: row.get(18)?,
        removing_at_ms: row.get(19)?,
        removed_at_ms: row.get(20)?,
    })
}

pub(super) fn change_mutation(
    transaction: &rusqlite::Transaction<'_>,
    change: Change,
    now_ms: i64,
) -> Result<ChangeMutation> {
    let event = FactoryEvent::ChangeChanged {
        change: change.snapshot(),
    };
    let sequence = append_event(transaction, now_ms, &event)?;
    Ok(ChangeMutation {
        change,
        event: Some(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        }),
    })
}

fn validate_reservation(input: &ChangeReservation, task_incarnation_id: &str) -> Result<()> {
    if input.max_factory_changes == 0 {
        return Err(StoreError::InvalidChangeCapacity);
    }
    if task_incarnation_id.is_empty()
        || task_incarnation_id.len() > MAX_TASK_INCARNATION_BYTES
        || !valid_absolute_path(&input.source_root)
    {
        return Err(StoreError::InvalidChangeMetadata);
    }
    Ok(())
}

fn validate_source_identity(identity: &ChangeSourceIdentity) -> Result<()> {
    if !valid_absolute_path(&identity.source_root) || identity.inode == 0 {
        return Err(StoreError::InvalidChangeMetadata);
    }
    sqlite_u64(identity.device)?;
    sqlite_positive_u64(identity.inode)?;
    sqlite_u64(identity.size_bytes)?;
    Ok(())
}

fn valid_base_identity(identity: &ChangeBaseIdentity) -> bool {
    valid_absolute_path(&identity.repository_root)
        && identity.inode != 0
        && sqlite_u64(identity.device).is_ok()
        && sqlite_positive_u64(identity.inode).is_ok()
}

fn validate_revision(value: i64) -> Result<()> {
    if value < 0 {
        return Err(StoreError::InvalidChangeMetadata);
    }
    Ok(())
}

pub(super) fn parse_change_phase(value: &str) -> Option<ChangePhase> {
    Some(match value {
        "provisioning" => ChangePhase::Provisioning,
        "available" => ChangePhase::Available,
        "removing" => ChangePhase::Removing,
        "removed" => ChangePhase::Removed,
        _ => return None,
    })
}

fn valid_absolute_path(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_PATH_BYTES && Path::new(value).is_absolute()
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn sqlite_u64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidChangeMetadata)
}

fn sqlite_positive_u64(value: u64) -> Result<i64> {
    if value == 0 {
        return Err(StoreError::InvalidChangeMetadata);
    }
    sqlite_u64(value)
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

fn parse_u64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(error))
    })
}

fn parse_optional_u64(value: Option<i64>, column: usize) -> rusqlite::Result<Option<u64>> {
    value.map(|value| parse_u64(value, column)).transpose()
}

fn invalid_value(column: usize, value: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        Type::Text,
        format!("invalid Change value {value:?}").into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewProject, NewTask};

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    fn fixture(store: &mut Store) -> (ProjectId, TaskId, String, ChangeId) {
        let project_id = ProjectId::try_from("project-1").unwrap();
        let task_id = TaskId::try_from("task-1").unwrap();
        let change_id = ChangeId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Project".into(),
                    root: "/tmp/project-1".into(),
                },
                1,
            )
            .unwrap();
        store
            .create_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Task".into(),
                    body: String::new(),
                    priority: 0,
                },
                2,
            )
            .unwrap();
        let incarnation: String = store
            .connection
            .query_row(
                "SELECT incarnation_id FROM tasks WHERE id = ?1",
                [task_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        (project_id, task_id, incarnation, change_id)
    }

    fn reservation(change_id: ChangeId, source_root: &str) -> ChangeReservation {
        ChangeReservation {
            id: change_id,
            source_root: source_root.into(),
            max_factory_changes: 1,
        }
    }

    fn reserve(
        store: &mut Store,
        project_id: &ProjectId,
        task_id: &TaskId,
        incarnation: &str,
        reservation: &ChangeReservation,
        now_ms: i64,
    ) -> Result<Change> {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mutation = reserve_change(
            &transaction,
            project_id,
            task_id,
            incarnation,
            reservation,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(mutation.change)
    }

    fn base() -> ChangeBaseIdentity {
        ChangeBaseIdentity {
            repository_root: "/tmp/project-1".into(),
            device: 7,
            inode: 11,
        }
    }

    fn source() -> ChangeSourceIdentity {
        ChangeSourceIdentity {
            source_root: "/tmp/change-1".into(),
            device: 8,
            inode: 12,
            size_bytes: 13,
        }
    }

    #[test]
    fn exact_revision_and_both_filesystem_identities_guard_change_lifecycle() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, task_id, incarnation, change_id) = fixture(&mut store);
        let declared = reserve(
            &mut store,
            &project_id,
            &task_id,
            &incarnation,
            &reservation(change_id.clone(), "/tmp/change-1"),
            3,
        )
        .unwrap();
        assert_eq!(declared.phase, ChangePhase::Provisioning);
        assert_eq!(declared.revision, 0);
        assert_eq!(store.recoverable_changes().unwrap(), vec![declared]);

        let based = store
            .record_change_base(&project_id, &change_id, 0, OID, &base(), 4)
            .unwrap()
            .change;
        assert_eq!(based.revision, 1);
        assert_eq!(based.base_oid.as_deref(), Some(OID));
        let repeated = store
            .record_change_base(&project_id, &change_id, 0, OID, &base(), 5)
            .unwrap();
        assert_eq!(repeated.change, based);
        assert!(repeated.event.is_none());

        let available = store
            .mark_change_available(
                &project_id,
                &change_id,
                1,
                &ChangeMaterialization {
                    base_oid: OID.into(),
                    base: base(),
                    source: source(),
                },
                6,
            )
            .unwrap()
            .change;
        assert_eq!(available.phase, ChangePhase::Available);
        assert_eq!(available.revision, 2);
        assert_eq!(available.measured_at_ms, Some(6));
        assert!(matches!(
            store.delete_task(&project_id, &task_id, 6),
            Err(StoreError::TaskHasChanges)
        ));
        assert!(matches!(
            store.check_project_deletable(&project_id),
            Err(StoreError::ProjectHasChanges)
        ));
        assert!(matches!(
            store.begin_change_removal(&project_id, &change_id, 1, 7),
            Err(StoreError::ChangeRevisionConflict)
        ));

        let removing = store
            .begin_change_removal(&project_id, &change_id, 2, 7)
            .unwrap()
            .change;
        assert_eq!(removing.phase, ChangePhase::Removing);
        let failed = store
            .record_change_failure(&project_id, &change_id, 3, "busy", 8)
            .unwrap()
            .change;
        assert_eq!(failed.phase, ChangePhase::Removing);
        assert_eq!(failed.last_failure.as_deref(), Some("busy"));
        let oversized = "é".repeat(MAX_CHANGE_FAILURE_BYTES);
        let bounded = store
            .record_change_failure(&project_id, &change_id, 4, &oversized, 9)
            .unwrap()
            .change;
        let persisted = bounded.last_failure.as_deref().unwrap();
        assert!(!persisted.is_empty());
        assert!(persisted.len() <= MAX_CHANGE_FAILURE_BYTES);
        assert!(persisted.is_char_boundary(persisted.len()));
        assert_eq!(store.recoverable_changes().unwrap(), vec![bounded]);

        let removed = store
            .mark_change_removed(&project_id, &change_id, 5, 10)
            .unwrap()
            .change;
        assert_eq!(removed.phase, ChangePhase::Removed);
        assert_eq!(removed.size_bytes, Some(13));
        assert_eq!(removed.measured_at_ms, Some(6));
        let change_events = store
            .events_after(0, 100)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.event {
                FactoryEvent::ChangeChanged { change } => Some(change),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            change_events
                .iter()
                .map(|change| change.revision)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5, 6]
        );
        assert!(
            change_events
                .iter()
                .all(|change| change.project_id == project_id && change.task_id == task_id)
        );
        let indexed: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE kind = 'change_changed' AND project_id = ?1 AND task_id = ?2
                   AND agent_id IS NULL AND run_id IS NULL",
                params![project_id.as_str(), task_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 7);
        let snapshots = store.list_changes(&project_id, None, 2).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].phase, ChangePhase::Removed);
        assert_eq!(snapshots[0].measured_bytes, Some(13));
        assert_eq!(
            store.change_storage_summary(Some(&project_id)).unwrap(),
            ChangeStorageSummary {
                retained_count: 0,
                measured_bytes: 0,
                measured_at_ms: None,
                active_leases: 0,
                complete: true,
            }
        );

        store
            .connection
            .execute(
                "UPDATE tasks SET status = 'failed' WHERE id = ?1",
                [task_id.as_str()],
            )
            .unwrap();
        assert!(matches!(
            store.retry_task(&project_id, &task_id, 10),
            Err(StoreError::TaskChangeUnavailable)
        ));
        store.delete_task(&project_id, &task_id, 11).unwrap();
        assert_eq!(store.change(&project_id, &change_id).unwrap(), None);
        assert_eq!(
            store
                .events_after(0, 100)
                .unwrap()
                .into_iter()
                .filter(|event| matches!(event.event, FactoryEvent::ChangeChanged { .. }))
                .count(),
            7
        );
    }

    #[test]
    fn quiescent_provisioning_measurement_is_reported_without_claiming_availability() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, task_id, incarnation, change_id) = fixture(&mut store);
        reserve(
            &mut store,
            &project_id,
            &task_id,
            &incarnation,
            &reservation(change_id.clone(), "/tmp/change-1"),
            3,
        )
        .unwrap();

        let measured = store
            .record_provisioning_measurement(&project_id, &change_id, 0, 8192, 4)
            .unwrap()
            .change;
        assert_eq!(measured.phase, ChangePhase::Provisioning);
        assert_eq!(measured.size_bytes, Some(8192));
        assert_eq!(
            store.change_storage_summary(Some(&project_id)).unwrap(),
            ChangeStorageSummary {
                retained_count: 1,
                measured_bytes: 8192,
                measured_at_ms: Some(4),
                active_leases: 0,
                complete: true,
            }
        );
    }

    #[test]
    fn explicit_removal_abandons_a_failed_provisioning_change_and_frees_capacity() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, task_id, incarnation, change_id) = fixture(&mut store);
        reserve(
            &mut store,
            &project_id,
            &task_id,
            &incarnation,
            &reservation(change_id.clone(), "/tmp/change-1"),
            3,
        )
        .unwrap();
        store
            .connection
            .execute(
                "UPDATE tasks SET status = 'failed' WHERE id = ?1",
                [task_id.as_str()],
            )
            .unwrap();

        let removing = store
            .begin_change_removal(&project_id, &change_id, 0, 4)
            .unwrap()
            .change;
        assert_eq!(removing.phase, ChangePhase::Removing);
        assert_eq!(
            removing.removal_kind().unwrap(),
            ChangeRemovalKind::Provisioning
        );
        assert!(matches!(
            store.retry_task(&project_id, &task_id, 5),
            Err(StoreError::TaskChangeUnavailable)
        ));

        let removed = store
            .mark_change_removed(&project_id, &change_id, 1, 6)
            .unwrap()
            .change;
        assert_eq!(removed.phase, ChangePhase::Removed);
        assert_eq!(
            removed.removal_kind().unwrap(),
            ChangeRemovalKind::Provisioning
        );
        assert_eq!(
            store.change_storage_summary(Some(&project_id)).unwrap(),
            ChangeStorageSummary {
                retained_count: 0,
                measured_bytes: 0,
                measured_at_ms: None,
                active_leases: 0,
                complete: true,
            }
        );
        assert!(store.check_project_deletable(&project_id).is_ok());
        store.delete_project(&project_id, 7).unwrap();
        assert_eq!(store.change(&project_id, &change_id).unwrap(), None);
    }

    #[test]
    fn quiescent_measurement_returns_and_persists_the_same_projection() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, task_id, incarnation, change_id) = fixture(&mut store);
        reserve(
            &mut store,
            &project_id,
            &task_id,
            &incarnation,
            &reservation(change_id.clone(), "/tmp/change-1"),
            3,
        )
        .unwrap();
        store
            .record_change_base(&project_id, &change_id, 0, OID, &base(), 4)
            .unwrap();
        store
            .mark_change_available(
                &project_id,
                &change_id,
                1,
                &ChangeMaterialization {
                    base_oid: OID.into(),
                    base: base(),
                    source: source(),
                },
                5,
            )
            .unwrap();

        let mut measured = source();
        measured.size_bytes = 21;
        let mutation = store
            .record_change_measurement(&project_id, &change_id, 2, &measured, 6)
            .unwrap();
        let projected = match mutation.event.unwrap().event {
            FactoryEvent::ChangeChanged { change } => change,
            event => panic!("unexpected event: {event:?}"),
        };
        assert_eq!(projected, mutation.change.snapshot());
        assert_eq!(projected.revision, 3);
        assert_eq!(projected.measured_bytes, Some(21));
        assert_eq!(projected.measured_at_ms, Some(6));
        assert!(matches!(
            store.events_after(0, 100).unwrap().last(),
            Some(EventEnvelope {
                event: FactoryEvent::ChangeChanged { change },
                ..
            }) if change == &projected
        ));
    }

    #[test]
    fn provisioning_and_failure_evidence_survive_store_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("factory.db");
        let (project_id, change_id, expected) = {
            let mut store = Store::open(&database).unwrap();
            let (project_id, task_id, incarnation, change_id) = fixture(&mut store);
            reserve(
                &mut store,
                &project_id,
                &task_id,
                &incarnation,
                &reservation(change_id.clone(), "/tmp/change-1"),
                3,
            )
            .unwrap();
            store
                .record_change_base(&project_id, &change_id, 0, OID, &base(), 4)
                .unwrap();
            let expected = store
                .record_change_failure(&project_id, &change_id, 1, "archive failed", 5)
                .unwrap()
                .change;
            (project_id, change_id, expected)
        };

        let store = Store::open(&database).unwrap();
        assert_eq!(
            store.change(&project_id, &change_id).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(store.recoverable_changes().unwrap(), vec![expected]);
    }

    #[test]
    fn declaration_enforces_factory_count_cap_before_any_external_effect() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, task_id, incarnation, change_id) = fixture(&mut store);
        reserve(
            &mut store,
            &project_id,
            &task_id,
            &incarnation,
            &reservation(change_id, "/tmp/change-1"),
            3,
        )
        .unwrap();
        let other_project_id = ProjectId::try_from("project-2").unwrap();
        store
            .create_project(
                NewProject {
                    id: other_project_id.clone(),
                    name: "Other".into(),
                    root: "/tmp/project-2".into(),
                },
                4,
            )
            .unwrap();
        let task_id = TaskId::try_from("task-2").unwrap();
        store
            .create_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: other_project_id.clone(),
                    parent_task_id: None,
                    title: "Second".into(),
                    body: String::new(),
                    priority: 0,
                },
                4,
            )
            .unwrap();
        let incarnation: String = store
            .connection
            .query_row(
                "SELECT incarnation_id FROM tasks WHERE id = ?1",
                [task_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(matches!(
            reserve(
                &mut store,
                &other_project_id,
                &task_id,
                &incarnation,
                &reservation(
                    ChangeId::try_from("22222222-2222-4222-8222-222222222222").unwrap(),
                    "/tmp/change-2",
                ),
                5,
            ),
            Err(StoreError::ChangeCapacityReached { limit: 1 })
        ));
    }
}
