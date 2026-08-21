use factory_core::{
    CompletionVerification, EventEnvelope, FactoryEvent, PROTOCOL_VERSION, ProjectId,
    ProjectSnapshot, RunId, RunOutcome,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{MAX_PATH_BYTES, Result, Store, StoreError};

const DIGEST_HEX_LEN: usize = 64;
const MAX_FAILURE_BYTES: usize = 4096;
pub(crate) const MAX_RUST_CACHE_COUNT: u64 = 8;
pub(crate) const MAX_RUST_CACHE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustCompletionPhase {
    Pending,
    Running,
    Passed,
    Failed,
}

impl RustCompletionPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "passed" => Self::Passed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Passed | Self::Failed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustCompletionCheck {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub project_incarnation_id: String,
    pub change_id: factory_core::ChangeId,
    pub phase: RustCompletionPhase,
    pub cache_key: Option<String>,
    pub source_digest: Option<String>,
    pub bundle_digest: Option<String>,
    pub failure: Option<String>,
    pub revision: i64,
    pub requested_at_ms: i64,
    pub updated_at_ms: i64,
    pub terminal_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustCacheLifecycle {
    Declared,
    Available,
    Reclaiming,
}

impl RustCacheLifecycle {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "declared" => Self::Declared,
            "available" => Self::Available,
            "reclaiming" => Self::Reclaiming,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustBuildCache {
    pub project_id: ProjectId,
    pub project_incarnation_id: String,
    pub cache_key: String,
    pub path: String,
    pub dev: Option<u64>,
    pub inode: Option<u64>,
    pub bytes: Option<u64>,
    pub lifecycle: RustCacheLifecycle,
    pub failure: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_used_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustStorageSummary {
    pub cache_count: u64,
    pub cache_bytes: Option<u64>,
    pub protected_count: u64,
    pub reclaimable_count: u64,
    pub failed_count: u64,
}

impl Store {
    pub fn set_project_completion_verification(
        &mut self,
        project_id: &ProjectId,
        verification: CompletionVerification,
        now_ms: i64,
    ) -> Result<(ProjectSnapshot, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: String = transaction
            .query_row(
                "SELECT completion_verification FROM projects WHERE id = ?1",
                [project_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::ProjectNotFound)?;
        if parse_verification(&current, 0)? != verification {
            let has_nonterminal_run: bool = transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM runs WHERE project_id = ?1 AND phase <> 'terminal'
                 )",
                [project_id.as_str()],
                |row| row.get(0),
            )?;
            if has_nonterminal_run {
                return Err(StoreError::ProjectHasActiveRun);
            }
        }
        let changed = transaction.execute(
            "UPDATE projects
             SET completion_verification = ?1, updated_at_ms = ?2
             WHERE id = ?3",
            params![verification_str(verification), now_ms, project_id.as_str()],
        )?;
        if changed != 1 {
            return Err(StoreError::ProjectNotFound);
        }
        let project = transaction.query_row(
            "SELECT id, name, root, completion_verification, created_at_ms, updated_at_ms
             FROM projects WHERE id = ?1",
            [project_id.as_str()],
            |row| {
                let verification: String = row.get(3)?;
                Ok(ProjectSnapshot {
                    id: super::parse_id(row.get(0)?, 0)?,
                    name: row.get(1)?,
                    root: row.get(2)?,
                    completion_verification: parse_verification(&verification, 3)?,
                    created_at_ms: row.get(4)?,
                    updated_at_ms: row.get(5)?,
                })
            },
        )?;
        let event = FactoryEvent::ProjectChanged {
            project: project.clone(),
        };
        let sequence = super::append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            project,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn rust_completion_check(&self, run_id: &RunId) -> Result<Option<RustCompletionCheck>> {
        load_check(&self.connection, run_id)
    }

    pub fn recoverable_rust_completion_checks(&self) -> Result<Vec<RustCompletionCheck>> {
        let mut statement = self.connection.prepare(
            "SELECT run_id FROM rust_completion_checks
             WHERE phase NOT IN ('passed', 'failed')
             ORDER BY requested_at_ms, run_id",
        )?;
        let ids = statement
            .query_map([], |row| super::parse_id::<RunId>(row.get(0)?, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|run_id| {
                load_check(&self.connection, &run_id)?
                    .ok_or(StoreError::RustCompletionCheckNotFound)
            })
            .collect()
    }

    /// Claims the one mutable cache writer for this project/configuration.
    /// The fixed key excludes source revision and is immutable after this call.
    pub fn claim_rust_completion_check(
        &mut self,
        run_id: &RunId,
        cache_key: &str,
        now_ms: i64,
    ) -> Result<RustCompletionCheck> {
        validate_digest(cache_key)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let check =
            load_check(&transaction, run_id)?.ok_or(StoreError::RustCompletionCheckNotFound)?;
        if check.phase != RustCompletionPhase::Pending {
            return Err(StoreError::InvalidRustCompletionPhase);
        }
        let busy: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM rust_completion_checks
                 WHERE project_incarnation_id = ?1 AND cache_key = ?2
                   AND phase = 'running'
                   AND run_id <> ?3
             )",
            params![check.project_incarnation_id, cache_key, run_id.as_str()],
            |row| row.get(0),
        )?;
        if busy {
            return Err(StoreError::RustCacheWriterBusy);
        }
        let existing = load_cache(&transaction, &check.project_incarnation_id, cache_key)?;
        if existing
            .as_ref()
            .and_then(|cache| cache.bytes)
            .is_some_and(|bytes| bytes > MAX_RUST_CACHE_BYTES)
        {
            return Err(StoreError::RustStorageCapacityReached {
                kind: "cache-bytes",
                limit: MAX_RUST_CACHE_BYTES,
            });
        }
        match existing {
            Some(cache)
                if matches!(
                    cache.lifecycle,
                    RustCacheLifecycle::Declared | RustCacheLifecycle::Available
                ) => {}
            existing => {
                ensure_cache_claim_capacity(&transaction)?;
                if existing.is_some() {
                    return Err(StoreError::InvalidRustBuildMetadata);
                }
            }
        }
        let changed = transaction.execute(
            "UPDATE rust_completion_checks
             SET phase = 'running', cache_key = ?1,
                 revision = revision + 1, updated_at_ms = ?2
             WHERE run_id = ?3 AND phase = 'pending' AND revision = ?4",
            params![cache_key, now_ms, run_id.as_str(), check.revision],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidRustCompletionPhase);
        }
        transaction.execute(
            "UPDATE rust_build_caches
             SET bytes = NULL, updated_at_ms = ?1
             WHERE project_incarnation_id = ?2 AND cache_key = ?3
               AND lifecycle IN ('declared', 'available')",
            params![now_ms, check.project_incarnation_id, cache_key],
        )?;
        let check =
            load_check(&transaction, run_id)?.ok_or(StoreError::RustCompletionCheckNotFound)?;
        transaction.commit()?;
        Ok(check)
    }

    pub fn pass_rust_completion_check(
        &mut self,
        run_id: &RunId,
        expected_revision: i64,
        source_digest: &str,
        bundle_digest: &str,
        now_ms: i64,
    ) -> Result<RustCompletionCheck> {
        validate_digest(source_digest)?;
        validate_digest(bundle_digest)?;
        let prepared: bool = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM rust_completion_checks c
                 JOIN rust_build_caches cache
                   ON cache.project_incarnation_id = c.project_incarnation_id
                  AND cache.cache_key = c.cache_key
                 WHERE c.run_id = ?1 AND c.phase = 'running' AND c.revision = ?2
                   AND cache.lifecycle = 'available' AND cache.bytes IS NOT NULL
             )",
            params![run_id.as_str(), expected_revision],
            |row| row.get(0),
        )?;
        if !prepared {
            return Err(StoreError::InvalidRustBuildMetadata);
        }
        self.transition_rust_check(
            run_id,
            expected_revision,
            RustCompletionPhase::Running,
            RustCompletionPhase::Passed,
            Some(source_digest),
            Some(bundle_digest),
            None,
            now_ms,
        )
    }

    pub fn fail_rust_completion_check(
        &mut self,
        run_id: &RunId,
        expected_revision: i64,
        failure: &str,
        now_ms: i64,
    ) -> Result<RustCompletionCheck> {
        validate_failure(failure)?;
        let current =
            load_check(&self.connection, run_id)?.ok_or(StoreError::RustCompletionCheckNotFound)?;
        if current.phase.is_terminal() || current.revision != expected_revision {
            return Err(StoreError::InvalidRustCompletionPhase);
        }
        self.transition_rust_check(
            run_id,
            expected_revision,
            current.phase,
            RustCompletionPhase::Failed,
            None,
            None,
            Some(failure),
            now_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn transition_rust_check(
        &mut self,
        run_id: &RunId,
        expected_revision: i64,
        expected: RustCompletionPhase,
        next: RustCompletionPhase,
        source_digest: Option<&str>,
        bundle_digest: Option<&str>,
        failure: Option<&str>,
        now_ms: i64,
    ) -> Result<RustCompletionCheck> {
        let terminal_at_ms = next.is_terminal().then_some(now_ms);
        let changed = self.connection.execute(
            "UPDATE rust_completion_checks
             SET phase = ?1,
                 source_digest = COALESCE(?2, source_digest),
                 bundle_digest = COALESCE(?3, bundle_digest),
                 failure = ?4, revision = revision + 1, updated_at_ms = ?5,
                 terminal_at_ms = ?6
             WHERE run_id = ?7 AND phase = ?8 AND revision = ?9",
            params![
                next.as_str(),
                source_digest,
                bundle_digest,
                failure,
                now_ms,
                terminal_at_ms,
                run_id.as_str(),
                expected.as_str(),
                expected_revision,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidRustCompletionPhase);
        }
        load_check(&self.connection, run_id)?.ok_or(StoreError::RustCompletionCheckNotFound)
    }

    pub fn declare_rust_cache(
        &mut self,
        run_id: &RunId,
        path: &str,
        now_ms: i64,
    ) -> Result<RustBuildCache> {
        validate_absolute_path(path)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let check =
            load_check(&transaction, run_id)?.ok_or(StoreError::RustCompletionCheckNotFound)?;
        let cache_key = check
            .cache_key
            .as_deref()
            .ok_or(StoreError::InvalidRustCompletionPhase)?;
        if !matches!(check.phase, RustCompletionPhase::Running) {
            return Err(StoreError::InvalidRustCompletionPhase);
        }
        let cache = declare_cache_row(
            &transaction,
            &check.project_id,
            &check.project_incarnation_id,
            cache_key,
            path,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(cache)
    }

    /// Binds the daemon-declared cache to an exact filesystem identity.
    /// A writer never inherits a pre-write byte measurement.
    pub fn bind_rust_cache_identity(
        &mut self,
        run_id: &RunId,
        path: &str,
        dev: u64,
        inode: u64,
        now_ms: i64,
    ) -> Result<RustBuildCache> {
        validate_bound_identity(path, inode)?;
        let check =
            load_check(&self.connection, run_id)?.ok_or(StoreError::RustCompletionCheckNotFound)?;
        let cache_key = check
            .cache_key
            .as_deref()
            .ok_or(StoreError::InvalidRustCompletionPhase)?;
        if check.phase != RustCompletionPhase::Running {
            return Err(StoreError::InvalidRustCompletionPhase);
        }
        let changed = self.connection.execute(
            "UPDATE rust_build_caches
             SET dev = ?1, inode = ?2, bytes = NULL, lifecycle = 'available',
                 failure = NULL, updated_at_ms = ?3, last_used_at_ms = ?3
             WHERE project_incarnation_id = ?4 AND cache_key = ?5
               AND path = ?6
               AND (lifecycle = 'declared' OR
                    (lifecycle = 'available' AND dev = ?1 AND inode = ?2))",
            params![
                to_i64(dev)?,
                to_i64(inode)?,
                now_ms,
                check.project_incarnation_id,
                cache_key,
                path,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidRustBuildMetadata);
        }
        load_cache(&self.connection, &check.project_incarnation_id, cache_key)?
            .ok_or(StoreError::InvalidRustBuildMetadata)
    }

    /// Records the exact cache size only after the writer process group has
    /// been reaped. Check revision and filesystem identity make crash retries
    /// idempotent without allowing an earlier writer to measure a newer one.
    #[allow(clippy::too_many_arguments)]
    pub fn record_rust_cache_measurement(
        &mut self,
        run_id: &RunId,
        expected_revision: i64,
        path: &str,
        dev: u64,
        inode: u64,
        bytes: u64,
        now_ms: i64,
    ) -> Result<RustBuildCache> {
        validate_bound_identity(path, inode)?;
        let changed = self.connection.execute(
            "UPDATE rust_build_caches AS cache
             SET bytes = ?1, failure = NULL, updated_at_ms = ?2,
                 last_used_at_ms = ?2
             WHERE cache.path = ?3 AND cache.dev = ?4 AND cache.inode = ?5
               AND cache.lifecycle = 'available'
               AND EXISTS (
                    SELECT 1 FROM rust_completion_checks check_row
                    WHERE check_row.run_id = ?6 AND check_row.phase = 'running'
                      AND check_row.revision = ?7
                      AND check_row.project_incarnation_id = cache.project_incarnation_id
                      AND check_row.cache_key = cache.cache_key
               )",
            params![
                to_i64(bytes)?,
                now_ms,
                path,
                to_i64(dev)?,
                to_i64(inode)?,
                run_id.as_str(),
                expected_revision,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidRustBuildMetadata);
        }
        let check =
            load_check(&self.connection, run_id)?.ok_or(StoreError::RustCompletionCheckNotFound)?;
        let cache_key = check
            .cache_key
            .as_deref()
            .ok_or(StoreError::InvalidRustCompletionPhase)?;
        load_cache(&self.connection, &check.project_incarnation_id, cache_key)?
            .ok_or(StoreError::InvalidRustBuildMetadata)
    }

    /// Exact persisted inventory. Byte totals are absent when any live row is
    /// not measured, rather than presenting a plausible partial total.
    pub fn rust_storage_summary(&self) -> Result<RustStorageSummary> {
        let (cache_count, cache_measured, cache_bytes): (i64, i64, i64) =
            self.connection.query_row(
                "SELECT COUNT(*), COUNT(bytes), COALESCE(SUM(bytes), 0)
             FROM rust_build_caches",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let protected_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM rust_build_caches c
             WHERE EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = c.project_incarnation_id
                  AND r.cache_key = c.cache_key AND r.phase = 'running')",
            [],
            |row| row.get(0),
        )?;
        let reclaimable_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM rust_build_caches c
             WHERE c.lifecycle = 'available' AND c.bytes IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = c.project_incarnation_id
                  AND r.cache_key = c.cache_key AND r.phase = 'running')",
            [],
            |row| row.get(0),
        )?;
        let failed_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM rust_build_caches WHERE failure IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(RustStorageSummary {
            cache_count: from_i64(cache_count, 0)?,
            cache_bytes: (cache_count == cache_measured)
                .then(|| from_i64(cache_bytes, 2))
                .transpose()?,
            protected_count: from_i64(protected_count, 0)?,
            reclaimable_count: from_i64(reclaimable_count, 0)?,
            failed_count: from_i64(failed_count, 0)?,
        })
    }

    /// Oldest unleased, identity-bound regenerable artifacts first. The
    /// caller still has to claim an exact row before touching the filesystem.
    pub fn rust_reclaim_candidates(&self, limit: usize) -> Result<Vec<RustBuildCache>> {
        if limit == 0 || limit > 1024 {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT c.project_incarnation_id, c.cache_key
             FROM rust_build_caches c
             WHERE c.lifecycle = 'available' AND c.bytes IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = c.project_incarnation_id
                  AND r.cache_key = c.cache_key AND r.phase = 'running'
             ) ORDER BY c.last_used_at_ms, c.cache_key LIMIT ?1",
        )?;
        let keys = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        keys.into_iter()
            .map(|(incarnation, key)| {
                load_cache(&self.connection, &incarnation, &key)?
                    .ok_or(StoreError::InvalidRustBuildMetadata)
            })
            .collect()
    }

    /// Reclaim intents that survived a daemon crash. These retain their exact
    /// registered identity and must be reconciled before selecting new work.
    pub fn recoverable_rust_reclaims(&self) -> Result<Vec<RustBuildCache>> {
        let mut output = Vec::new();
        let mut caches = self.connection.prepare(
            "SELECT project_incarnation_id, cache_key FROM rust_build_caches
             WHERE lifecycle = 'reclaiming' AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = rust_build_caches.project_incarnation_id
                  AND r.cache_key = rust_build_caches.cache_key AND r.phase = 'running'
             )
             ORDER BY updated_at_ms, project_incarnation_id, cache_key",
        )?;
        let cache_keys = caches
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(caches);
        for (incarnation, key) in cache_keys {
            output.push(
                load_cache(&self.connection, &incarnation, &key)?
                    .ok_or(StoreError::InvalidRustBuildMetadata)?,
            );
        }
        Ok(output)
    }

    /// Artifacts declared before an external mkdir/publish effect but not yet
    /// bound. Restart keeps a failed declaration recoverable and visible;
    /// it is never silently treated as reclaimable.
    pub fn recoverable_rust_cache_declarations(&self) -> Result<Vec<RustBuildCache>> {
        let mut output = Vec::new();
        let mut caches = self.connection.prepare(
            "SELECT project_incarnation_id, cache_key FROM rust_build_caches
             WHERE lifecycle = 'declared' AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = rust_build_caches.project_incarnation_id
                  AND r.cache_key = rust_build_caches.cache_key AND r.phase = 'running'
             )
             ORDER BY updated_at_ms, project_incarnation_id, cache_key",
        )?;
        let cache_keys = caches
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(caches);
        for (incarnation, key) in cache_keys {
            output.push(
                load_cache(&self.connection, &incarnation, &key)?
                    .ok_or(StoreError::InvalidRustBuildMetadata)?,
            );
        }
        Ok(output)
    }

    pub fn begin_rust_cache_reclaim(&mut self, cache: &RustBuildCache, now_ms: i64) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE rust_build_caches SET lifecycle = 'reclaiming', updated_at_ms = ?1
             WHERE project_incarnation_id = ?2 AND cache_key = ?3
               AND path = ?4 AND dev = ?5 AND inode = ?6 AND bytes = ?7
               AND lifecycle IN ('available', 'reclaiming') AND NOT EXISTS (
                    SELECT 1 FROM rust_completion_checks r
                    WHERE r.project_incarnation_id = rust_build_caches.project_incarnation_id
                      AND r.cache_key = rust_build_caches.cache_key AND r.phase = 'running'
               )",
            params![
                now_ms,
                cache.project_incarnation_id,
                cache.cache_key,
                cache.path,
                cache.dev.map(to_i64).transpose()?,
                cache.inode.map(to_i64).transpose()?,
                cache.bytes.map(to_i64).transpose()?,
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::InvalidRustBuildMetadata)
        }
    }

    pub fn finish_rust_cache_reclaim(
        &mut self,
        incarnation_id: &str,
        cache_key: &str,
    ) -> Result<()> {
        finish_reclaim(&self.connection, incarnation_id, cache_key)
    }

    pub fn record_rust_cache_failure(
        &mut self,
        incarnation_id: &str,
        cache_key: &str,
        failure: &str,
        now_ms: i64,
    ) -> Result<()> {
        fail_reclaim(&self.connection, incarnation_id, cache_key, failure, now_ms)
    }

    /// Completes reconciliation after an exact filesystem absence check.
    pub fn finish_absent_declared_rust_cache(
        &mut self,
        incarnation_id: &str,
        cache_key: &str,
    ) -> Result<()> {
        finish_absent_declaration(&self.connection, incarnation_id, cache_key)
    }

    /// Atomically turns a deletable project's exact regenerable caches into
    /// durable reclaim intents. Ambiguous declarations remain explicit
    /// blockers.
    pub fn begin_project_rust_cache_reclamation(
        &mut self,
        project_id: &ProjectId,
        now_ms: i64,
    ) -> Result<u64> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
            [project_id.as_str()],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::ProjectNotFound);
        }
        let has_nonterminal_run: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM runs WHERE project_id = ?1 AND phase <> 'terminal'
             )",
            [project_id.as_str()],
            |row| row.get(0),
        )?;
        if has_nonterminal_run {
            return Err(StoreError::ProjectHasActiveRun);
        }
        let has_change_metadata: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM changes WHERE project_id = ?1 AND phase <> 'removed'
                UNION ALL
                SELECT 1 FROM legacy_sources WHERE project_id = ?1
             )",
            [project_id.as_str()],
            |row| row.get(0),
        )?;
        if has_change_metadata {
            return Err(StoreError::ProjectHasChanges);
        }
        let has_ambiguous_cache: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM rust_build_caches
                WHERE project_id = ?1 AND lifecycle = 'declared'
             )",
            [project_id.as_str()],
            |row| row.get(0),
        )?;
        if has_ambiguous_cache {
            return Err(StoreError::ProjectHasRustCaches);
        }
        let changed = transaction.execute(
            "UPDATE rust_build_caches
             SET lifecycle = 'reclaiming', updated_at_ms = ?1
             WHERE project_id = ?2 AND lifecycle = 'available' AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = rust_build_caches.project_incarnation_id
                  AND r.cache_key = rust_build_caches.cache_key AND r.phase = 'running'
             )",
            params![now_ms, project_id.as_str()],
        )?;
        transaction.commit()?;
        u64::try_from(changed).map_err(|_| StoreError::InvalidRustBuildMetadata)
    }
}

fn declare_cache_row(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    incarnation_id: &str,
    cache_key: &str,
    path: &str,
    now_ms: i64,
) -> Result<RustBuildCache> {
    if let Some(existing) = load_cache(transaction, incarnation_id, cache_key)? {
        if existing.path != path {
            return Err(StoreError::InvalidRustBuildMetadata);
        }
        match existing.lifecycle {
            RustCacheLifecycle::Declared | RustCacheLifecycle::Available => {
                transaction.execute(
                    "UPDATE rust_build_caches
                     SET updated_at_ms = ?1, last_used_at_ms = ?1
                     WHERE project_incarnation_id = ?2 AND cache_key = ?3",
                    params![now_ms, incarnation_id, cache_key],
                )?;
            }
            RustCacheLifecycle::Reclaiming => {
                return Err(StoreError::InvalidRustBuildMetadata);
            }
        }
    } else {
        ensure_artifact_capacity(transaction)?;
        transaction.execute(
            "INSERT INTO rust_build_caches (
                project_incarnation_id, cache_key, project_id, path,
                dev, inode, bytes, lifecycle, failure,
                created_at_ms, updated_at_ms, last_used_at_ms
             ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 'declared', NULL, ?5, ?5, ?5)",
            params![incarnation_id, cache_key, project_id.as_str(), path, now_ms,],
        )?;
    }
    load_cache(transaction, incarnation_id, cache_key)?.ok_or(StoreError::InvalidRustBuildMetadata)
}

fn ensure_artifact_capacity(transaction: &Transaction<'_>) -> Result<()> {
    let count: i64 =
        transaction.query_row("SELECT COUNT(*) FROM rust_build_caches", [], |row| {
            row.get(0)
        })?;
    if from_i64(count, 0)? >= MAX_RUST_CACHE_COUNT {
        return Err(StoreError::RustStorageCapacityReached {
            kind: "cache",
            limit: MAX_RUST_CACHE_COUNT,
        });
    }
    Ok(())
}

fn ensure_cache_claim_capacity(transaction: &Transaction<'_>) -> Result<()> {
    let claimed: i64 = transaction.query_row(
        "SELECT
            (SELECT COUNT(*) FROM rust_build_caches)
            + (SELECT COUNT(*) FROM rust_completion_checks r
               WHERE r.phase = 'running' AND NOT EXISTS (
                   SELECT 1 FROM rust_build_caches c
                   WHERE c.project_incarnation_id = r.project_incarnation_id
                     AND c.cache_key = r.cache_key
               ))",
        [],
        |row| row.get(0),
    )?;
    if from_i64(claimed, 0)? >= MAX_RUST_CACHE_COUNT {
        return Err(StoreError::RustStorageCapacityReached {
            kind: "cache",
            limit: MAX_RUST_CACHE_COUNT,
        });
    }
    Ok(())
}

pub(super) fn insert_completion_check_if_required(
    transaction: &Transaction<'_>,
    run: &factory_core::RunSnapshot,
    proposal: &RunOutcome,
    now_ms: i64,
) -> Result<()> {
    if !matches!(proposal, RunOutcome::Succeeded) {
        return Ok(());
    }
    let (role, verification, incarnation_id, change_id): (String, String, String, Option<String>) =
        transaction.query_row(
            "SELECT a.role, p.completion_verification, p.incarnation_id, r.change_id
         FROM runs r
         JOIN agents a ON a.id = r.agent_id AND a.project_id = r.project_id
         JOIN projects p ON p.id = r.project_id
         WHERE r.id = ?1",
            [run.id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    if role != "worker" || parse_verification(&verification, 1)? == CompletionVerification::None {
        return Ok(());
    }
    let change_id = change_id.ok_or(StoreError::InvalidRunState)?;
    transaction.execute(
        "INSERT INTO rust_completion_checks (
            run_id, project_id, project_incarnation_id, change_id, phase,
            cache_key, source_digest, bundle_digest, failure, revision,
            requested_at_ms, updated_at_ms, terminal_at_ms
         ) VALUES (?1, ?2, ?3, ?4, 'pending', NULL, NULL, NULL, NULL, 0, ?5, ?5, NULL)",
        params![
            run.id.as_str(),
            run.project_id.as_str(),
            incarnation_id,
            change_id,
            now_ms,
        ],
    )?;
    Ok(())
}

fn load_check(
    connection: &rusqlite::Connection,
    run_id: &RunId,
) -> Result<Option<RustCompletionCheck>> {
    connection
        .query_row(
            "SELECT run_id, project_id, project_incarnation_id, change_id, phase,
                    cache_key, source_digest, bundle_digest, failure, revision,
                    requested_at_ms, updated_at_ms, terminal_at_ms
             FROM rust_completion_checks WHERE run_id = ?1",
            [run_id.as_str()],
            |row| {
                let phase: String = row.get(4)?;
                Ok(RustCompletionCheck {
                    run_id: super::parse_id(row.get(0)?, 0)?,
                    project_id: super::parse_id(row.get(1)?, 1)?,
                    project_incarnation_id: row.get(2)?,
                    change_id: super::parse_id(row.get(3)?, 3)?,
                    phase: RustCompletionPhase::parse(&phase).ok_or_else(|| {
                        rusqlite::Error::InvalidColumnType(4, phase, rusqlite::types::Type::Text)
                    })?,
                    cache_key: row.get(5)?,
                    source_digest: row.get(6)?,
                    bundle_digest: row.get(7)?,
                    failure: row.get(8)?,
                    revision: row.get(9)?,
                    requested_at_ms: row.get(10)?,
                    updated_at_ms: row.get(11)?,
                    terminal_at_ms: row.get(12)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn load_cache(
    connection: &rusqlite::Connection,
    incarnation_id: &str,
    cache_key: &str,
) -> Result<Option<RustBuildCache>> {
    connection
        .query_row(
            "SELECT project_id, project_incarnation_id, cache_key, path,
                    dev, inode, bytes, lifecycle, failure,
                    created_at_ms, updated_at_ms, last_used_at_ms
             FROM rust_build_caches
             WHERE project_incarnation_id = ?1 AND cache_key = ?2",
            params![incarnation_id, cache_key],
            |row| {
                let lifecycle: String = row.get(7)?;
                Ok(RustBuildCache {
                    project_id: super::parse_id(row.get(0)?, 0)?,
                    project_incarnation_id: row.get(1)?,
                    cache_key: row.get(2)?,
                    path: row.get(3)?,
                    dev: optional_u64(row.get(4)?, 4)?,
                    inode: optional_u64(row.get(5)?, 5)?,
                    bytes: optional_u64(row.get(6)?, 6)?,
                    lifecycle: parse_lifecycle(&lifecycle, 7)?,
                    failure: row.get(8)?,
                    created_at_ms: row.get(9)?,
                    updated_at_ms: row.get(10)?,
                    last_used_at_ms: row.get(11)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn finish_reclaim(
    connection: &rusqlite::Connection,
    incarnation_id: &str,
    cache_key: &str,
) -> Result<()> {
    let changed = connection.execute(
        "DELETE FROM rust_build_caches
         WHERE project_incarnation_id = ?1 AND cache_key = ?2
           AND lifecycle = 'reclaiming' AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = rust_build_caches.project_incarnation_id
                  AND r.cache_key = rust_build_caches.cache_key AND r.phase = 'running'
           )",
        params![incarnation_id, cache_key],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::InvalidRustBuildMetadata)
    }
}

fn finish_absent_declaration(
    connection: &rusqlite::Connection,
    incarnation_id: &str,
    cache_key: &str,
) -> Result<()> {
    let changed = connection.execute(
        "DELETE FROM rust_build_caches
         WHERE project_incarnation_id = ?1 AND cache_key = ?2
           AND lifecycle = 'declared' AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = rust_build_caches.project_incarnation_id
                  AND r.cache_key = rust_build_caches.cache_key AND r.phase = 'running'
           )",
        params![incarnation_id, cache_key],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::InvalidRustBuildMetadata)
    }
}

fn fail_reclaim(
    connection: &rusqlite::Connection,
    incarnation_id: &str,
    cache_key: &str,
    failure: &str,
    now_ms: i64,
) -> Result<()> {
    validate_failure(failure)?;
    let changed = connection.execute(
        "UPDATE rust_build_caches
         SET failure = ?1, updated_at_ms = ?2
         WHERE project_incarnation_id = ?3 AND cache_key = ?4
           AND lifecycle IN ('declared', 'reclaiming') AND NOT EXISTS (
                SELECT 1 FROM rust_completion_checks r
                WHERE r.project_incarnation_id = rust_build_caches.project_incarnation_id
                  AND r.cache_key = rust_build_caches.cache_key AND r.phase = 'running'
           )",
        params![failure, now_ms, incarnation_id, cache_key],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::InvalidRustBuildMetadata)
    }
}

fn parse_verification(value: &str, column: usize) -> rusqlite::Result<CompletionVerification> {
    match value {
        "none" => Ok(CompletionVerification::None),
        "rust_workspace_test" => Ok(CompletionVerification::RustWorkspaceTest),
        _ => Err(rusqlite::Error::InvalidColumnType(
            column,
            value.to_owned(),
            rusqlite::types::Type::Text,
        )),
    }
}

const fn verification_str(value: CompletionVerification) -> &'static str {
    match value {
        CompletionVerification::None => "none",
        CompletionVerification::RustWorkspaceTest => "rust_workspace_test",
    }
}

fn parse_lifecycle(value: &str, column: usize) -> rusqlite::Result<RustCacheLifecycle> {
    RustCacheLifecycle::parse(value).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(column, value.to_owned(), rusqlite::types::Type::Text)
    })
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() == DIGEST_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(StoreError::InvalidRustBuildMetadata)
    }
}

fn validate_failure(value: &str) -> Result<()> {
    if !value.is_empty() && value.len() <= MAX_FAILURE_BYTES {
        Ok(())
    } else {
        Err(StoreError::InvalidRustBuildMetadata)
    }
}

fn validate_absolute_path(value: &str) -> Result<()> {
    if !value.is_empty()
        && value.len() <= MAX_PATH_BYTES
        && std::path::Path::new(value).is_absolute()
    {
        Ok(())
    } else {
        Err(StoreError::InvalidRustBuildMetadata)
    }
}

fn validate_bound_identity(path: &str, inode: u64) -> Result<()> {
    validate_absolute_path(path)?;
    if inode == 0 {
        Err(StoreError::InvalidRustBuildMetadata)
    } else {
        Ok(())
    }
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidRustBuildMetadata)
}

fn from_i64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn optional_u64(value: Option<i64>, column: usize) -> rusqlite::Result<Option<u64>> {
    value.map(|value| from_i64(value, column)).transpose()
}

#[cfg(test)]
mod tests {
    use factory_core::ProjectId;

    use super::*;
    use crate::store::NewProject;

    const CACHE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CACHE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CACHE_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const CACHE_D: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn project(store: &mut Store) -> (ProjectId, String) {
        let project_id = ProjectId::try_from("factory").unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Factory".into(),
                    root: "/tmp/factory".into(),
                },
                1,
            )
            .unwrap();
        let incarnation = store
            .connection
            .query_row(
                "SELECT incarnation_id FROM projects WHERE id = ?1",
                [project_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        (project_id, incarnation)
    }

    fn cache_key(index: u64) -> String {
        format!("{index:064x}")
    }

    fn run_id(index: u64) -> RunId {
        RunId::try_from(format!("00000000-0000-4000-8000-{index:012x}"))
            .expect("generated test RunId should be valid")
    }

    fn declare_cache(
        store: &mut Store,
        project_id: &ProjectId,
        incarnation: &str,
        key: &str,
        path: &str,
        now_ms: i64,
    ) -> Result<RustBuildCache> {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cache = declare_cache_row(&transaction, project_id, incarnation, key, path, now_ms)?;
        transaction.commit()?;
        Ok(cache)
    }

    fn insert_test_check(
        store: &Store,
        run_id: &RunId,
        project_id: &ProjectId,
        incarnation: &str,
        phase: RustCompletionPhase,
        cache_key: Option<&str>,
    ) {
        // These unit tests exercise the build-ledger predicates without
        // recreating the much larger attempt-kernel fixture.
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = OFF")
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO rust_completion_checks (
                    run_id, project_id, project_incarnation_id, change_id, phase,
                    cache_key, source_digest, bundle_digest, failure, revision,
                    requested_at_ms, updated_at_ms, terminal_at_ms
                 ) VALUES (?1, ?2, ?3, 'test-change', ?4, ?5, NULL, NULL, NULL, 0, 2, 2, NULL)",
                params![
                    run_id.as_str(),
                    project_id.as_str(),
                    incarnation,
                    phase.as_str(),
                    cache_key,
                ],
            )
            .unwrap();
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .unwrap();
    }

    #[test]
    fn capacity_refuses_claim_without_consuming_pending_and_allows_exact_reuse() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, incarnation) = project(&mut store);
        declare_cache(
            &mut store,
            &project_id,
            &incarnation,
            CACHE_A,
            "/tmp/cache-a",
            3,
        )
        .unwrap();
        for index in 1..MAX_RUST_CACHE_COUNT {
            declare_cache(
                &mut store,
                &project_id,
                &incarnation,
                &cache_key(index),
                &format!("/tmp/cache-{index}"),
                3,
            )
            .unwrap();
        }
        // Exact identities are reusable even when the inventory is at cap.
        declare_cache(
            &mut store,
            &project_id,
            &incarnation,
            CACHE_A,
            "/tmp/cache-a",
            4,
        )
        .unwrap();
        let run_id = RunId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        insert_test_check(
            &store,
            &run_id,
            &project_id,
            &incarnation,
            RustCompletionPhase::Pending,
            None,
        );
        assert!(matches!(
            store.claim_rust_completion_check(&run_id, CACHE_B, 6),
            Err(StoreError::RustStorageCapacityReached {
                kind: "cache",
                limit: MAX_RUST_CACHE_COUNT
            })
        ));
        assert_eq!(
            store.rust_completion_check(&run_id).unwrap().unwrap().phase,
            RustCompletionPhase::Pending
        );
        assert_eq!(
            store
                .claim_rust_completion_check(&run_id, CACHE_A, 7)
                .unwrap()
                .phase,
            RustCompletionPhase::Running
        );
    }

    #[test]
    fn running_claim_reserves_capacity_before_the_cache_path_exists() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, incarnation) = project(&mut store);
        for index in 0..MAX_RUST_CACHE_COUNT {
            insert_test_check(
                &store,
                &run_id(index),
                &project_id,
                &incarnation,
                RustCompletionPhase::Running,
                Some(&cache_key(index)),
            );
        }
        let second_run = RunId::try_from("44444444-4444-4444-8444-444444444444").unwrap();
        insert_test_check(
            &store,
            &second_run,
            &project_id,
            &incarnation,
            RustCompletionPhase::Pending,
            None,
        );

        assert!(matches!(
            store.claim_rust_completion_check(&second_run, CACHE_B, 3),
            Err(StoreError::RustStorageCapacityReached {
                kind: "cache",
                limit: MAX_RUST_CACHE_COUNT
            })
        ));
        assert_eq!(
            store
                .rust_completion_check(&second_run)
                .unwrap()
                .unwrap()
                .phase,
            RustCompletionPhase::Pending
        );
    }

    #[test]
    fn claim_refuses_oversized_cache_and_clears_reused_measurement() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, incarnation) = project(&mut store);
        declare_cache(
            &mut store,
            &project_id,
            &incarnation,
            CACHE_A,
            "/tmp/cache-a",
            3,
        )
        .unwrap();
        store
            .connection
            .execute(
                "UPDATE rust_build_caches
                 SET lifecycle = 'available', dev = 1, inode = 2, bytes = ?1
                 WHERE cache_key = ?2",
                params![to_i64(MAX_RUST_CACHE_BYTES + 1).unwrap(), CACHE_A],
            )
            .unwrap();
        let run_id = RunId::try_from("55555555-5555-4555-8555-555555555555").unwrap();
        insert_test_check(
            &store,
            &run_id,
            &project_id,
            &incarnation,
            RustCompletionPhase::Pending,
            None,
        );
        assert!(matches!(
            store.claim_rust_completion_check(&run_id, CACHE_A, 4),
            Err(StoreError::RustStorageCapacityReached {
                kind: "cache-bytes",
                limit: MAX_RUST_CACHE_BYTES
            })
        ));
        assert_eq!(
            store.rust_completion_check(&run_id).unwrap().unwrap().phase,
            RustCompletionPhase::Pending
        );
        store
            .connection
            .execute(
                "UPDATE rust_build_caches SET bytes = 1 WHERE cache_key = ?1",
                [CACHE_A],
            )
            .unwrap();
        store
            .claim_rust_completion_check(&run_id, CACHE_A, 6)
            .unwrap();
        assert_eq!(
            load_cache(&store.connection, &incarnation, CACHE_A)
                .unwrap()
                .unwrap()
                .bytes,
            None
        );
        assert_eq!(store.rust_storage_summary().unwrap().cache_bytes, None);
    }

    #[test]
    fn cache_is_incomplete_until_exact_post_reap_measurement() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, incarnation) = project(&mut store);
        let run_id = RunId::try_from("66666666-6666-4666-8666-666666666666").unwrap();
        insert_test_check(
            &store,
            &run_id,
            &project_id,
            &incarnation,
            RustCompletionPhase::Running,
            Some(CACHE_A),
        );

        let declared = store
            .declare_rust_cache(&run_id, "/tmp/cache-a", 3)
            .unwrap();
        assert_eq!(declared.lifecycle, RustCacheLifecycle::Declared);
        assert_eq!(declared.bytes, None);
        assert_eq!(store.rust_storage_summary().unwrap().cache_bytes, None);

        let bound = store
            .bind_rust_cache_identity(&run_id, "/tmp/cache-a", 1, 2, 4)
            .unwrap();
        assert_eq!(bound.lifecycle, RustCacheLifecycle::Available);
        assert_eq!(bound.bytes, None);
        assert!(matches!(
            store.pass_rust_completion_check(&run_id, 0, CACHE_B, CACHE_C, 5),
            Err(StoreError::InvalidRustBuildMetadata)
        ));
        assert!(matches!(
            store.record_rust_cache_measurement(&run_id, 1, "/tmp/cache-a", 1, 2, 7, 5),
            Err(StoreError::InvalidRustBuildMetadata)
        ));

        let measured = store
            .record_rust_cache_measurement(&run_id, 0, "/tmp/cache-a", 1, 2, 7, 6)
            .unwrap();
        assert_eq!(measured.bytes, Some(7));
        assert_eq!(
            store
                .record_rust_cache_measurement(&run_id, 0, "/tmp/cache-a", 1, 2, 7, 7)
                .unwrap()
                .bytes,
            Some(7)
        );
        assert_eq!(store.rust_storage_summary().unwrap().cache_bytes, Some(7));
        assert_eq!(
            store
                .pass_rust_completion_check(&run_id, 0, CACHE_B, CACHE_C, 8)
                .unwrap()
                .phase,
            RustCompletionPhase::Passed
        );
    }

    #[test]
    fn project_cache_reclamation_requires_exact_unambiguous_inventory() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, incarnation) = project(&mut store);
        declare_cache(
            &mut store,
            &project_id,
            &incarnation,
            CACHE_A,
            "/tmp/cache-a",
            2,
        )
        .unwrap();
        assert!(matches!(
            store.begin_project_rust_cache_reclamation(&project_id, 3),
            Err(StoreError::ProjectHasRustCaches)
        ));
        store
            .record_rust_cache_failure(&incarnation, CACHE_A, "identity unknown", 4)
            .unwrap();
        assert_eq!(store.rust_storage_summary().unwrap().failed_count, 1);
        assert!(matches!(
            store.begin_project_rust_cache_reclamation(&project_id, 5),
            Err(StoreError::ProjectHasRustCaches)
        ));

        let mut store = Store::open_in_memory().unwrap();
        let (project_id, incarnation) = project(&mut store);
        declare_cache(
            &mut store,
            &project_id,
            &incarnation,
            CACHE_A,
            "/tmp/cache-a",
            2,
        )
        .unwrap();
        store
            .connection
            .execute(
                "UPDATE rust_build_caches
                 SET lifecycle = 'available', dev = 1, inode = 2, bytes = 3",
                [],
            )
            .unwrap();
        assert_eq!(
            store
                .begin_project_rust_cache_reclamation(&project_id, 3)
                .unwrap(),
            1
        );
        assert_eq!(store.recoverable_rust_reclaims().unwrap().len(), 1);
    }

    #[test]
    fn recovery_and_reclaim_protect_only_exact_live_artifacts() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, incarnation) = project(&mut store);
        for (key, path) in [
            (CACHE_A, "/tmp/cache-a"),
            (CACHE_B, "/tmp/cache-b"),
            (CACHE_C, "/tmp/cache-c"),
            (CACHE_D, "/tmp/cache-d"),
        ] {
            declare_cache(&mut store, &project_id, &incarnation, key, path, 2).unwrap();
        }
        let run_id = RunId::try_from("22222222-2222-4222-8222-222222222222").unwrap();
        insert_test_check(
            &store,
            &run_id,
            &project_id,
            &incarnation,
            RustCompletionPhase::Running,
            Some(CACHE_A),
        );

        let recoverable = store.recoverable_rust_cache_declarations().unwrap();
        assert_eq!(recoverable.len(), 3);
        assert!(!recoverable.iter().any(|cache| cache.cache_key == CACHE_A));
        assert!(matches!(
            store.record_rust_cache_failure(&incarnation, CACHE_A, "missing", 3),
            Err(StoreError::InvalidRustBuildMetadata)
        ));
        store
            .record_rust_cache_failure(&incarnation, CACHE_B, "missing", 3)
            .unwrap();
        store
            .finish_absent_declared_rust_cache(&incarnation, CACHE_C)
            .unwrap();
        assert!(
            load_cache(&store.connection, &incarnation, CACHE_C)
                .unwrap()
                .is_none()
        );

        store
            .connection
            .execute(
                "UPDATE rust_build_caches
                 SET lifecycle = 'available', dev = 1, inode = 2, bytes = 3
                 WHERE cache_key IN (?1, ?2)",
                params![CACHE_A, CACHE_D],
            )
            .unwrap();
        assert_eq!(store.rust_storage_summary().unwrap().protected_count, 1);
        assert_eq!(store.rust_reclaim_candidates(8).unwrap().len(), 1);
        let cache = load_cache(&store.connection, &incarnation, CACHE_A)
            .unwrap()
            .unwrap();
        assert!(matches!(
            store.begin_rust_cache_reclaim(&cache, 4),
            Err(StoreError::InvalidRustBuildMetadata)
        ));
        let cache = load_cache(&store.connection, &incarnation, CACHE_D)
            .unwrap()
            .unwrap();
        store.begin_rust_cache_reclaim(&cache, 4).unwrap();
        store
            .record_rust_cache_failure(&incarnation, CACHE_D, "identity changed", 5)
            .unwrap();
        assert!(store.rust_reclaim_candidates(8).unwrap().is_empty());
    }
}
