package kernel

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"path/filepath"
)

func validateDurableControls(ctx context.Context, connection *sql.Conn) error {
	state, err := validateDurableEntityControls(ctx, connection)
	if err != nil {
		return err
	}
	return validateInvalidations(ctx, connection, state)
}

// validateDurableEntityControls validates every durable authority row except
// the invalidation log itself, which validateDurableControls then checks.
func validateDurableEntityControls(ctx context.Context, connection *sql.Conn) (FactoryState, error) {
	state, err := factoryState(ctx, connection)
	if err != nil {
		return FactoryState{}, err
	}
	if err := validateProjects(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate projects: %w", err)
	}
	if err := validateAgents(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate agents: %w", err)
	}
	if err := validateTasks(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate tasks: %w", err)
	}
	if err := validateChanges(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate Changes: %w", err)
	}
	if err := validateRuns(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate runs: %w", err)
	}
	if err := validateResources(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate resources: %w", err)
	}
	if err := validateOwnershipLocators(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate ownership locators: %w", err)
	}
	if err := validateRunRelationships(ctx, connection); err != nil {
		return FactoryState{}, fmt.Errorf("validate run relationships: %w", err)
	}
	if err := validateBrowserAuthority(ctx, connection); err != nil {
		return FactoryState{}, err
	}
	if err := validateHumanRequests(ctx, connection); err != nil {
		return FactoryState{}, err
	}
	if err := validateResourceIdentityCollisions(ctx, connection); err != nil {
		return FactoryState{}, err
	}
	return state, nil
}

func validateHumanRequests(ctx context.Context, connection *sql.Conn) error {
	var unresolved int64
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM human_requests WHERE status IN ('open', 'delivering', 'delivery_unknown')`).Scan(&unresolved); err != nil {
		return err
	}
	if unresolved < 0 || unresolved > MaxOpenHumanRequests {
		return fmt.Errorf("%w: human request bound exceeded", ErrCorruptState)
	}
	rows, err := connection.QueryContext(ctx, `SELECT `+humanRequestColumns+` FROM human_requests ORDER BY id`)
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		request, found, scanErr := scanHumanRequest(rows)
		if scanErr != nil || !found {
			if scanErr == nil {
				scanErr = ErrCorruptState
			}
			return scanErr
		}
		var phase string
		if err := connection.QueryRowContext(ctx, `SELECT phase FROM runs WHERE id = ?`, request.RunID.Bytes()).Scan(&phase); err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				return fmt.Errorf("%w: human request run is missing", ErrCorruptState)
			}
			return err
		}
		validPhase := false
		switch request.Status {
		case HumanRequestOpen, HumanRequestDelivering:
			validPhase = phase == RunRunning.String()
		case HumanRequestDeliveryUnknown:
			// Recovery can discover an uncertain delivery while the run is
			// still running; finalization preserves that uncertainty until
			// terminalization makes it stale.
			validPhase = phase == RunRunning.String() || phase == RunFinalizing.String()
		case HumanRequestResolved:
			// A reply may resolve before the run later enters finalizing or
			// terminal. The request remains historical after that point.
			validPhase = phase == RunRunning.String() || phase == RunFinalizing.String() || phase == RunTerminal.String()
		case HumanRequestStale:
			validPhase = phase == RunFinalizing.String() || phase == RunTerminal.String()
		}
		if !validPhase {
			return fmt.Errorf("%w: human request status has impossible run phase", ErrCorruptState)
		}
	}
	return rows.Err()
}

// validateBrowserAuthority is intentionally a concrete integrity pass. Browser
// rows are credentials and leases, not a general permission/state framework.
func validateBrowserAuthority(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT id FROM browser_clients ORDER BY id`)
	if err != nil {
		return err
	}
	for rows.Next() {
		var raw []byte
		if err := rows.Scan(&raw); err != nil {
			rows.Close()
			return err
		}
		id, err := BrowserClientIDFromBytes(raw)
		if err != nil {
			rows.Close()
			return fmt.Errorf("%w: invalid browser client identity", ErrCorruptState)
		}
		if _, found, err := browserClientByID(ctx, connection, id); err != nil || !found {
			rows.Close()
			if err == nil {
				err = ErrCorruptState
			}
			return err
		}
	}
	if err := closeValidatedBrowserRows(rows); err != nil {
		return err
	}
	rows, err = connection.QueryContext(ctx, `SELECT secret_digest FROM browser_pairing_challenges ORDER BY secret_digest`)
	if err != nil {
		return err
	}
	for rows.Next() {
		var raw []byte
		if err := rows.Scan(&raw); err != nil {
			rows.Close()
			return err
		}
		digest, err := BrowserChallengeDigestFromBytes(raw)
		if err != nil {
			rows.Close()
			return fmt.Errorf("%w: invalid browser challenge identity", ErrCorruptState)
		}
		if _, found, err := browserChallengeByDigest(ctx, connection, digest); err != nil || !found {
			rows.Close()
			if err == nil {
				err = ErrCorruptState
			}
			return err
		}
	}
	if err := closeValidatedBrowserRows(rows); err != nil {
		return err
	}
	var challengeCount int64
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM browser_pairing_challenges`).Scan(&challengeCount); err != nil {
		return err
	}
	if challengeCount < 0 || challengeCount > 32 {
		return fmt.Errorf("%w: browser pairing challenge retention exceeded", ErrCorruptState)
	}
	var count int64
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM browser_security_events`).Scan(&count); err != nil {
		return err
	}
	if count > EventRetentionLimit {
		return fmt.Errorf("%w: browser security event retention exceeded", ErrCorruptState)
	}
	rows, err = connection.QueryContext(ctx, `SELECT sequence, kind, client_id, occurred_at_ms FROM browser_security_events ORDER BY sequence`)
	if err != nil {
		return err
	}
	var previous int64
	for rows.Next() {
		var sequence, occurred int64
		var kind string
		var client nullableBlob
		if err := rows.Scan(&sequence, &kind, &client, &occurred); err != nil {
			rows.Close()
			return err
		}
		parsedKind := BrowserSecurityEventKind(kind)
		if sequence < 1 || sequence <= previous || !validBrowserSecurityKind(parsedKind) || isBrowserChallengeEvent(parsedKind) != !client.valid || occurred < 0 {
			rows.Close()
			return fmt.Errorf("%w: invalid browser security event", ErrCorruptState)
		}
		if client.valid {
			id, err := BrowserClientIDFromBytes(client.bytes)
			if err != nil {
				rows.Close()
				return fmt.Errorf("%w: invalid browser event client", ErrCorruptState)
			}
			if _, found, err := browserClientByID(ctx, connection, id); err != nil || !found {
				rows.Close()
				if err == nil {
					err = ErrCorruptState
				}
				return err
			}
		}
		previous = sequence
	}
	if err := closeValidatedBrowserRows(rows); err != nil {
		return err
	}
	rows, err = connection.QueryContext(ctx, `SELECT id, run_id, state, lease_client_id, lease_generation, lease_expires_at_ms, last_input_sequence FROM terminal_sessions`)
	if err != nil {
		return err
	}
	for rows.Next() {
		var rawID, rawRun []byte
		var state string
		var client nullableBlob
		var generation, expiry, sequence sql.NullInt64
		if err := rows.Scan(&rawID, &rawRun, &state, &client, &generation, &expiry, &sequence); err != nil {
			rows.Close()
			return err
		}
		if !generation.Valid || !sequence.Valid || generation.Int64 < 0 || sequence.Int64 < 0 || expiry.Valid && expiry.Int64 < 0 {
			rows.Close()
			return fmt.Errorf("%w: invalid terminal lease controls", ErrCorruptState)
		}
		if client.valid {
			if state != "active" || !expiry.Valid || !generation.Valid || generation.Int64 < 1 {
				rows.Close()
				return fmt.Errorf("%w: lease on inactive terminal session", ErrCorruptState)
			}
			id, err := BrowserClientIDFromBytes(client.bytes)
			if err != nil {
				rows.Close()
				return fmt.Errorf("%w: invalid terminal lease client", ErrCorruptState)
			}
			browser, found, err := browserClientByID(ctx, connection, id)
			if err != nil || !found || browser.RevokedAt != nil || !browser.CapabilityMask.Has(BrowserCapabilityTerminalInput) {
				rows.Close()
				return fmt.Errorf("%w: terminal lease client is not active", ErrCorruptState)
			}
			var phase string
			if err := connection.QueryRowContext(ctx, `SELECT phase FROM runs WHERE id = ?`, rawRun).Scan(&phase); err != nil || phase != "running" {
				rows.Close()
				return fmt.Errorf("%w: terminal lease run is not running", ErrCorruptState)
			}
		} else if expiry.Valid || sequence.Int64 != 0 {
			rows.Close()
			return fmt.Errorf("%w: terminal lease nullability mismatch", ErrCorruptState)
		}
		if _, err := TerminalSessionIDFromBytes(rawID); err != nil {
			rows.Close()
			return fmt.Errorf("%w: invalid terminal session identity", ErrCorruptState)
		}
	}
	return closeValidatedBrowserRows(rows)
}

func closeValidatedBrowserRows(rows *sql.Rows) error {
	return errors.Join(rows.Err(), rows.Close())
}

func validateResourceIdentityCollisions(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT left_resource.run_id, left_resource.kind, right_resource.run_id, right_resource.kind
		FROM resources AS left_resource
		JOIN resources AS right_resource ON left_resource.id < right_resource.id AND (
			(left_resource.path_dev IS NOT NULL AND left_resource.path_dev = right_resource.path_dev AND left_resource.path_inode = right_resource.path_inode) OR
			(left_resource.pid IS NOT NULL AND left_resource.pid = right_resource.pid AND left_resource.pgid = right_resource.pgid AND left_resource.birth_digest = right_resource.birth_digest)
		)`)
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		var leftRunRaw, rightRunRaw []byte
		var leftKindRaw, rightKindRaw string
		if err := rows.Scan(&leftRunRaw, &leftKindRaw, &rightRunRaw, &rightKindRaw); err != nil {
			return err
		}
		leftRun, leftRunErr := RunIDFromBytes(leftRunRaw)
		rightRun, rightRunErr := RunIDFromBytes(rightRunRaw)
		leftKind, leftKindErr := parseResourceKind(leftKindRaw)
		rightKind, rightKindErr := parseResourceKind(rightKindRaw)
		providerPair := leftRun == rightRun && (leftKind == ResourceProviderProcess && rightKind == ResourceProviderGroup || leftKind == ResourceProviderGroup && rightKind == ResourceProviderProcess)
		if leftRunErr != nil || rightRunErr != nil || leftKindErr != nil || rightKindErr != nil || !providerPair {
			return fmt.Errorf("%w: resource identities collide", ErrCorruptState)
		}
	}
	return rows.Err()
}

func validateRunRelationships(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT `+runColumns+` FROM runs`)
	if err != nil {
		return err
	}
	var runs []Run
	for rows.Next() {
		run, found, err := scanRun(rows)
		if err != nil || !found {
			rows.Close()
			if err == nil {
				err = ErrCorruptState
			}
			return err
		}
		runs = append(runs, run)
	}
	if err := rows.Close(); err != nil {
		return err
	}
	for _, run := range runs {
		if _, err := loadRunRelationships(ctx, connection, run); err != nil {
			return err
		}
	}
	return nil
}

type runRelationships struct {
	task            Task
	change          *Change
	changeOwnership workerChangeOwnership
	resources       []Resource
	session         TerminalSession
}

type workerChangeOwnership uint8

type workerChangeProvenance uint8

const (
	workerChangeReserved workerChangeOwnership = iota + 1
	workerChangePrepared
	workerChangeAvailableRetained
	workerChangeAvailableFresh
	workerChangeSettledRetainedRetry
	workerChangeSettledRetainedFresh
	workerChangeSettledAbandonedReserved
	workerChangeSettledAbandonedPrepared
	workerChangeSettledAbandonedAvailableFresh
	workerChangeSettledAbandonedAvailableRetained
)

const (
	workerChangeFresh workerChangeProvenance = iota + 1
	workerChangeRetained
)

func (ownership workerChangeOwnership) available() bool {
	return ownership == workerChangeAvailableRetained || ownership == workerChangeAvailableFresh
}

func (ownership workerChangeOwnership) settled() bool {
	switch ownership {
	case workerChangeSettledRetainedRetry, workerChangeSettledRetainedFresh, workerChangeSettledAbandonedReserved, workerChangeSettledAbandonedPrepared,
		workerChangeSettledAbandonedAvailableFresh, workerChangeSettledAbandonedAvailableRetained:
		return true
	default:
		return false
	}
}

// canSettleAs says which settlements an owned Change accepts. An available
// Change retains, or is abandoned only by the daemon's refusal of its
// published tree; an unpublished one abandons.
func (ownership workerChangeOwnership) canSettleAs(phase ChangePhase, refused bool) bool {
	switch phase {
	case ChangeRetained:
		return ownership.available()
	case ChangeAbandoned:
		if refused {
			return ownership.available()
		}
		return ownership == workerChangeReserved || ownership == workerChangePrepared
	default:
		return false
	}
}

// refusedPublication reports a terminal run whose published tree the daemon
// refused: the failure only NewRefusedChangeSettlement writes.
func refusedPublication(run Run) bool {
	return run.Phase == RunTerminal && run.Terminal != nil && refusedProposal(*run.Terminal)
}

func classifyWorkerChangeOwnership(ctx context.Context, connection *sql.Conn, run Run, change Change) (workerChangeOwnership, error) {
	if run.Role != RoleWorker || run.ChangeID == nil || run.AdmittedChangeRevision == nil ||
		change.ID != *run.ChangeID || change.ProjectID != run.ProjectID || change.TaskID != run.TaskID || change.TaskIncarnationID != run.TaskIncarnationID {
		return 0, fmt.Errorf("%w: Change does not match worker run", ErrCorruptState)
	}
	admitted := run.AdmittedChangeRevision.Int64()
	if change.Revision.Int64() < admitted {
		return 0, fmt.Errorf("%w: Change predates worker admission", ErrCorruptState)
	}
	delta := change.Revision.Int64() - admitted
	provenance, err := workerChangeProvenanceForRun(ctx, connection, run)
	if err != nil {
		return 0, err
	}
	var ownership workerChangeOwnership
	switch {
	case change.Phase == ChangeReserved && delta == 0 && provenance == workerChangeFresh:
		ownership = workerChangeReserved
	case change.Phase == ChangePrepared && delta == 1 && provenance == workerChangeFresh:
		ownership = workerChangePrepared
	case change.Phase == ChangeAvailable && delta == 0 && provenance == workerChangeRetained:
		ownership = workerChangeAvailableRetained
	case change.Phase == ChangeAvailable && delta == 2 && provenance == workerChangeFresh:
		ownership = workerChangeAvailableFresh
	case change.Phase == ChangeRetained && delta == 1 && provenance == workerChangeRetained && change.SettledRunID != nil && *change.SettledRunID == run.ID:
		ownership = workerChangeSettledRetainedRetry
	case change.Phase == ChangeRetained && delta == 3 && provenance == workerChangeFresh && change.SettledRunID != nil && *change.SettledRunID == run.ID:
		ownership = workerChangeSettledRetainedFresh
	case change.Phase == ChangeAbandoned && delta == 1 && provenance == workerChangeFresh && change.SettledRunID != nil && *change.SettledRunID == run.ID:
		ownership = workerChangeSettledAbandonedReserved
	case change.Phase == ChangeAbandoned && delta == 2 && provenance == workerChangeFresh && change.SettledRunID != nil && *change.SettledRunID == run.ID:
		ownership = workerChangeSettledAbandonedPrepared
	case change.Phase == ChangeAbandoned && delta == 3 && provenance == workerChangeFresh && change.SettledRunID != nil && *change.SettledRunID == run.ID && refusedPublication(run):
		ownership = workerChangeSettledAbandonedAvailableFresh
	case change.Phase == ChangeAbandoned && delta == 1 && provenance == workerChangeRetained && change.SettledRunID != nil && *change.SettledRunID == run.ID && refusedPublication(run):
		ownership = workerChangeSettledAbandonedAvailableRetained
	default:
		return 0, fmt.Errorf("%w: impossible worker Change revision", ErrCorruptState)
	}
	if ownership.settled() {
		if run.Phase != RunTerminal {
			return 0, fmt.Errorf("%w: nonterminal worker settled Change", ErrCorruptState)
		}
		return ownership, nil
	}
	if run.Phase == RunTerminal || change.SettledRunID != nil {
		return 0, fmt.Errorf("%w: worker Change settlement does not match run", ErrCorruptState)
	}
	if run.RunningAt != nil {
		if !ownership.available() || change.AvailableAt == nil || change.AvailableAt.Int64() > run.RunningAt.Int64() || change.UpdatedAt.Int64() > run.RunningAt.Int64() {
			return 0, fmt.Errorf("%w: running worker does not own available Change", ErrCorruptState)
		}
	}
	return ownership, nil
}

// workerChangeProvenanceForRun proves an available-at-A admission with one forward
// automaton over the complete contiguous worker history. A fresh predecessor
// may introduce retained provenance with the exact +4 settle-and-reopen gap.
// Once retained, every later admission must use the exact +2 retained retry
// gap; another +4 can never reset or reintroduce that provenance. The one
// reset is a refused publication, which abandons the Change and reopens it
// reserved: +4 from a fresh predecessor or +2 from a retained one, and the
// retry is fresh either way.
func workerChangeProvenanceForRun(ctx context.Context, connection *sql.Conn, run Run) (workerChangeProvenance, error) {
	if run.AdmittedTaskWorkRevision.Int64() < 1 || run.AdmittedChangeRevision == nil || run.ChangeID == nil {
		return 0, fmt.Errorf("%w: incomplete worker Change history", ErrCorruptState)
	}
	rows, err := connection.QueryContext(ctx, `SELECT `+runColumns+` FROM runs WHERE task_id = ? AND task_incarnation_id = ? AND admitted_task_work_revision <= ? ORDER BY admitted_task_work_revision`,
		run.TaskID.Bytes(), run.TaskIncarnationID.Bytes(), run.AdmittedTaskWorkRevision.Int64())
	if err != nil {
		return 0, err
	}
	var history []Run
	for rows.Next() {
		value, found, scanErr := scanRun(rows)
		if scanErr != nil || !found {
			rows.Close()
			if scanErr == nil {
				scanErr = ErrCorruptState
			}
			return 0, scanErr
		}
		history = append(history, value)
	}
	if err := errors.Join(rows.Err(), rows.Close()); err != nil {
		return 0, err
	}
	if int64(len(history)) != run.AdmittedTaskWorkRevision.Int64() || history[len(history)-1].ID != run.ID || history[len(history)-1].AdmittedChangeRevision == nil || *history[len(history)-1].AdmittedChangeRevision != *run.AdmittedChangeRevision {
		return 0, fmt.Errorf("%w: noncontiguous worker Change history", ErrCorruptState)
	}
	provenance := workerChangeFresh
	for index, current := range history {
		workRevision := int64(index + 1)
		if current.Role != RoleWorker || current.ChangeID == nil || current.AdmittedChangeRevision == nil || *current.ChangeID != *run.ChangeID ||
			current.ProjectID != run.ProjectID || current.TaskID != run.TaskID || current.TaskIncarnationID != run.TaskIncarnationID || current.AdmittedTaskWorkRevision.Int64() != workRevision {
			return 0, fmt.Errorf("%w: inconsistent worker Change history", ErrCorruptState)
		}
		if workRevision == 1 {
			if current.AdmittedChangeRevision.Int64() != 1 {
				return 0, fmt.Errorf("%w: worker Change history does not start fresh", ErrCorruptState)
			}
			continue
		}
		previous := history[index-1]
		if previous.Phase != RunTerminal || previous.Role != RoleWorker || previous.ChangeID == nil || previous.AdmittedChangeRevision == nil || *previous.ChangeID != *run.ChangeID ||
			previous.ProjectID != run.ProjectID || previous.TaskID != run.TaskID || previous.TaskIncarnationID != run.TaskIncarnationID || previous.TerminalAt == nil || previous.TerminalAt.Int64() > current.AdmittedAt.Int64() {
			return 0, fmt.Errorf("%w: invalid worker Change predecessor", ErrCorruptState)
		}
		delta := current.AdmittedChangeRevision.Int64() - previous.AdmittedChangeRevision.Int64()
		if refusedPublication(previous) {
			// The daemon refused the predecessor's published tree and
			// abandoned the Change from available: +4 on the fresh path
			// (available, abandoned, reopened reserved), +2 on the retained
			// one. Either way the retry starts fresh on a reserved Change.
			if provenance == workerChangeRetained && delta != 2 || provenance == workerChangeFresh && delta != 4 {
				return 0, fmt.Errorf("%w: invalid refused Change retry gap", ErrCorruptState)
			}
			provenance = workerChangeFresh
			continue
		}
		if provenance == workerChangeRetained {
			if delta != 2 {
				return 0, fmt.Errorf("%w: invalid retained Change retry gap", ErrCorruptState)
			}
			continue
		}
		switch delta {
		case 2, 3:
			// A fresh abandoned predecessor stays on the fresh path.
		case 4:
			provenance = workerChangeRetained
		default:
			return 0, fmt.Errorf("%w: invalid fresh Change retry gap", ErrCorruptState)
		}
	}
	return provenance, nil
}

func loadRunRelationships(ctx context.Context, connection *sql.Conn, run Run) (runRelationships, error) {
	task, found, err := taskByID(ctx, connection, run.TaskID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return runRelationships{}, err
	}
	if task.ProjectID != run.ProjectID || task.IncarnationID != run.TaskIncarnationID {
		return runRelationships{}, fmt.Errorf("%w: task does not match run", ErrCorruptState)
	}
	if err := validateTaskRunTopology(ctx, connection, task); err != nil {
		return runRelationships{}, err
	}
	if run.Phase != RunTerminal {
		if !taskMatchesRun(task, run) {
			return runRelationships{}, fmt.Errorf("%w: task does not match run", ErrCorruptState)
		}
	} else if task.WorkRevision.Int64() < run.AdmittedTaskWorkRevision.Int64() || task.WorkRevision == run.AdmittedTaskWorkRevision && !taskMatchesRun(task, run) {
		return runRelationships{}, fmt.Errorf("%w: task does not match run", ErrCorruptState)
	}

	var change *Change
	var changeOwnership workerChangeOwnership
	if run.ChangeID != nil {
		value, found, err := changeByID(ctx, connection, *run.ChangeID)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return runRelationships{}, err
		}
		if run.Role != RoleWorker || run.AdmittedChangeRevision == nil || value.ProjectID != run.ProjectID || value.TaskID != run.TaskID || value.TaskIncarnationID != run.TaskIncarnationID {
			return runRelationships{}, fmt.Errorf("%w: Change does not match run", ErrCorruptState)
		}
		if run.Phase != RunTerminal {
			changeOwnership, err = classifyWorkerChangeOwnership(ctx, connection, run, value)
			if err != nil || changeOwnership.settled() {
				if err == nil {
					err = ErrCorruptState
				}
				return runRelationships{}, err
			}
		} else {
			var later int64
			if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM runs WHERE task_id = ? AND task_incarnation_id = ? AND admitted_task_work_revision > ?`, run.TaskID.Bytes(), run.TaskIncarnationID.Bytes(), run.AdmittedTaskWorkRevision.Int64()).Scan(&later); err != nil {
				return runRelationships{}, err
			}
			if later == 0 {
				changeOwnership, err = classifyWorkerChangeOwnership(ctx, connection, run, value)
				if err != nil || !changeOwnership.settled() {
					if err == nil {
						err = ErrCorruptState
					}
					return runRelationships{}, err
				}
				if run.TerminalAt == nil || value.UpdatedAt.Int64() > run.TerminalAt.Int64() {
					return runRelationships{}, fmt.Errorf("%w: terminal run predates Change settlement", ErrCorruptState)
				}
			}
		}
		change = &value
	} else if run.Role != RoleOrchestrator {
		return runRelationships{}, fmt.Errorf("%w: missing worker Change", ErrCorruptState)
	}

	resources, err := resourcesForRun(ctx, connection, run.ID)
	if err != nil {
		return runRelationships{}, err
	}
	if !resourcesMatchRunPhase(run.Phase, resources) {
		return runRelationships{}, fmt.Errorf("%w: resources do not match run phase", ErrCorruptState)
	}
	var providerProcessIdentity, providerGroupIdentity ResourceIdentity
	var providerProcessState, providerGroupState ResourceState
	var providerProcessReason, providerGroupReason string
	var providerProcessReleased, providerGroupReleased *UnixMillis
	for _, resource := range resources {
		switch resource.Kind {
		case ResourceProviderProcess:
			providerProcessIdentity = resource.Identity
			providerProcessState = resource.State
			providerProcessReason = resource.UnresolvedReason
			providerProcessReleased = resource.ReleasedAt
		case ResourceProviderGroup:
			providerGroupIdentity = resource.Identity
			providerGroupState = resource.State
			providerGroupReason = resource.UnresolvedReason
			providerGroupReleased = resource.ReleasedAt
		}
	}
	if !resourceIdentityEqual(providerProcessIdentity, providerGroupIdentity) || providerProcessState != providerGroupState || providerProcessReason != providerGroupReason ||
		(providerProcessReleased == nil) != (providerGroupReleased == nil) || providerProcessReleased != nil && *providerProcessReleased != *providerGroupReleased {
		return runRelationships{}, fmt.Errorf("%w: provider process/group identity is not atomic", ErrCorruptState)
	}
	var providerProcessActivation, providerGroupActivation *UnixMillis
	for _, resource := range resources {
		switch resource.Kind {
		case ResourceProviderProcess:
			providerProcessActivation = resource.ActivatedAt
		case ResourceProviderGroup:
			providerGroupActivation = resource.ActivatedAt
		}
	}
	if (providerProcessActivation == nil) != (providerGroupActivation == nil) || providerProcessActivation != nil && *providerProcessActivation != *providerGroupActivation {
		return runRelationships{}, fmt.Errorf("%w: provider process/group activation is not atomic", ErrCorruptState)
	}
	for _, resource := range resources {
		if resource.State != ResourceReleased || resource.Identity.Empty() {
			continue
		}
		switch resource.Kind {
		case ResourceProviderProcess, ResourceProviderGroup:
			if run.ProviderExit == nil {
				return runRelationships{}, fmt.Errorf("%w: released provider resource lacks exit evidence", ErrCorruptState)
			}
		case ResourceRunnerProcess:
			if run.RunnerExit == nil {
				return runRelationships{}, fmt.Errorf("%w: released runner resource lacks exit evidence", ErrCorruptState)
			}
		}
	}
	if err := validatePersistedProcessExits(run, resources); err != nil {
		return runRelationships{}, err
	}
	session, err := validateRunTerminalSession(ctx, connection, run)
	if err != nil {
		return runRelationships{}, err
	}
	if err := validateRunResourceChronology(run, change, resources); err != nil {
		return runRelationships{}, err
	}
	return runRelationships{task: task, change: change, changeOwnership: changeOwnership, resources: resources, session: session}, nil
}

func validateRunTerminalSession(ctx context.Context, connection *sql.Conn, run Run) (TerminalSession, error) {
	session, found, err := terminalSessionByRunID(ctx, connection, run.ID)
	if err != nil {
		return TerminalSession{}, err
	}
	if !found {
		return TerminalSession{}, fmt.Errorf("%w: run has no terminal session", ErrCorruptState)
	}
	if session.DeclaredAt != run.AdmittedAt || session.UpdatedAt.Int64() > run.UpdatedAt.Int64() {
		return TerminalSession{}, fmt.Errorf("%w: terminal session chronology does not match run", ErrCorruptState)
	}
	if session.ActivatedAt == nil {
		if run.RunningAt != nil {
			return TerminalSession{}, fmt.Errorf("%w: running run lacks terminal activation", ErrCorruptState)
		}
	} else if run.RunningAt == nil || *session.ActivatedAt != *run.RunningAt {
		return TerminalSession{}, fmt.Errorf("%w: terminal activation does not match run", ErrCorruptState)
	}
	if session.ClosedAt != nil && run.TerminalAt != nil && session.ClosedAt.Int64() > run.TerminalAt.Int64() {
		return TerminalSession{}, fmt.Errorf("%w: terminal session closes after run", ErrCorruptState)
	}
	switch run.Phase {
	case RunAdmitted:
		if session.State != TerminalSessionDeclared {
			return TerminalSession{}, fmt.Errorf("%w: admitted run terminal session is not declared", ErrCorruptState)
		}
	case RunRunning:
		if session.State != TerminalSessionActive {
			return TerminalSession{}, fmt.Errorf("%w: running run terminal session is not active", ErrCorruptState)
		}
	case RunFinalizing:
		if session.State != TerminalSessionReleasing && session.State != TerminalSessionUnresolved && session.State != TerminalSessionClosed {
			return TerminalSession{}, fmt.Errorf("%w: finalizing run terminal session has invalid phase", ErrCorruptState)
		}
	case RunTerminal:
		if session.State != TerminalSessionClosed {
			return TerminalSession{}, fmt.Errorf("%w: terminal run terminal session is not closed", ErrCorruptState)
		}
	}
	return session, nil
}

func validateRunResourceChronology(run Run, change *Change, resources []Resource) error {
	if run.Phase == RunAdmitted && run.UpdatedAt != run.AdmittedAt {
		var runner *Resource
		for index := range resources {
			if resources[index].Kind == ResourceRunnerProcess {
				runner = &resources[index]
				break
			}
		}
		if runner == nil || (runner.State != ResourceStarting && runner.State != ResourceActive) || runner.Revision != run.Revision || runner.UpdatedAt != run.UpdatedAt {
			return fmt.Errorf("%w: admitted run update lacks exact runner-start transition", ErrCorruptState)
		}
	}
	if run.Phase == RunRunning {
		if run.RunningAt == nil {
			return fmt.Errorf("%w: running run lacks running time", ErrCorruptState)
		}
		runningAt := run.RunningAt.Int64()
		if change != nil && (change.Phase != ChangeAvailable || change.AvailableAt == nil || change.AvailableAt.Int64() > runningAt || change.UpdatedAt.Int64() > runningAt) {
			return fmt.Errorf("%w: Change became available after run activation", ErrCorruptState)
		}
		for _, resource := range resources {
			if resource.State == ResourceActive && (resource.ActivatedAt == nil || resource.ActivatedAt.Int64() > runningAt || resource.UpdatedAt.Int64() > runningAt) {
				return fmt.Errorf("%w: active resource became current after run activation", ErrCorruptState)
			}
		}
	}
	if run.RunningAt != nil {
		runningAt := run.RunningAt.Int64()
		for _, resource := range resources {
			if resource.ActivatedAt != nil && resource.ActivatedAt.Int64() > runningAt {
				return fmt.Errorf("%w: resource activation follows run activation", ErrCorruptState)
			}
		}
	}
	if run.Phase == RunFinalizing || run.Phase == RunTerminal {
		if run.FinalizingAt == nil {
			return fmt.Errorf("%w: cleanup phase lacks finalizing time", ErrCorruptState)
		}
		finalizingAt := run.FinalizingAt.Int64()
		for _, resource := range resources {
			if resource.ActivatedAt != nil && resource.ActivatedAt.Int64() > finalizingAt {
				return fmt.Errorf("%w: resource activation follows finalizing", ErrCorruptState)
			}
			if resource.UpdatedAt.Int64() < finalizingAt {
				return fmt.Errorf("%w: resource update predates finalizing", ErrCorruptState)
			}
			if run.Phase == RunTerminal && resource.State == ResourceReleased && (resource.ReleasedAt == nil || resource.ReleasedAt.Int64() > run.TerminalAt.Int64()) {
				return fmt.Errorf("%w: resource release follows terminal run", ErrCorruptState)
			}
		}
	}
	return nil
}

func validatePersistedProcessExits(run Run, resources []Resource) error {
	// Exit rows carry no identity of their own. Their authority is the exact
	// activated resource (or provider process/group pair) still recorded here.
	var providerProcess, providerGroup, runnerProcess *Resource
	for index := range resources {
		resource := &resources[index]
		switch resource.Kind {
		case ResourceProviderProcess:
			providerProcess = resource
		case ResourceProviderGroup:
			providerGroup = resource
		case ResourceRunnerProcess:
			runnerProcess = resource
		}
	}
	if run.ProviderExit != nil {
		if providerProcess == nil || providerGroup == nil || providerProcess.ActivatedAt == nil || providerGroup.ActivatedAt == nil || providerProcess.Identity.Empty() || !resourceIdentityEqual(providerProcess.Identity, providerGroup.Identity) || *providerProcess.ActivatedAt != *providerGroup.ActivatedAt || run.ProviderExit.At().Int64() < providerProcess.ActivatedAt.Int64() {
			return fmt.Errorf("%w: provider exit does not match activated provider resources", ErrCorruptState)
		}
		for _, resource := range []*Resource{providerProcess, providerGroup} {
			if resource.ReleasedAt != nil && resource.ReleasedAt.Int64() < run.ProviderExit.At().Int64() {
				return fmt.Errorf("%w: provider resource released before provider exit", ErrCorruptState)
			}
		}
	}
	if run.RunnerExit != nil {
		if runnerProcess == nil || runnerProcess.ActivatedAt == nil || runnerProcess.Identity.Empty() || run.RunnerExit.At().Int64() < runnerProcess.ActivatedAt.Int64() {
			return fmt.Errorf("%w: runner exit does not match activated runner resource", ErrCorruptState)
		}
		if runnerProcess.ReleasedAt != nil && runnerProcess.ReleasedAt.Int64() < run.RunnerExit.At().Int64() {
			return fmt.Errorf("%w: runner resource released before runner exit", ErrCorruptState)
		}
	}
	return nil
}

func taskMatchesRun(task Task, run Run) bool {
	if run.Phase != RunTerminal {
		return task.AssignedAgentID == run.AgentID && task.WorkRevision == run.AdmittedTaskWorkRevision && task.Status == TaskRunning
	}
	if task.AssignedAgentID != run.AgentID || task.WorkRevision != run.AdmittedTaskWorkRevision || run.Terminal == nil || run.TerminalAt == nil || task.UpdatedAt != *run.TerminalAt {
		return false
	}
	switch run.Terminal.kind {
	case OutcomeSucceeded:
		return task.Status == TaskSucceeded && task.Result == run.Terminal.result && task.CompletedAt != nil && *task.CompletedAt == *run.TerminalAt
	case OutcomeBlocked:
		return task.Status == TaskBlocked && task.BlockedReason == run.Terminal.detail && task.CompletedAt == nil
	case OutcomeFailed:
		return task.Status == TaskFailed && task.CompletedAt != nil && *task.CompletedAt == *run.TerminalAt
	case OutcomeCancelled:
		return task.Status == TaskCancelled && task.CompletedAt != nil && *task.CompletedAt == *run.TerminalAt
	default:
		return false
	}
}

func validateTaskRunTopology(ctx context.Context, connection *sql.Conn, task Task) error {
	rows, err := connection.QueryContext(ctx, `SELECT `+runColumns+` FROM runs WHERE task_id = ? AND task_incarnation_id = ? ORDER BY admitted_task_work_revision`, task.ID.Bytes(), task.IncarnationID.Bytes())
	if err != nil {
		return err
	}
	expectedRevision := int64(1)
	var latest Run
	found := false
	var invalid error
	for rows.Next() {
		run, present, err := scanRun(rows)
		if err != nil || !present || run.ProjectID != task.ProjectID || run.TaskID != task.ID || run.TaskIncarnationID != task.IncarnationID || run.AdmittedTaskWorkRevision.Int64() != expectedRevision {
			if err != nil {
				invalid = err
			} else {
				invalid = fmt.Errorf("%w: invalid task/run revision topology", ErrCorruptState)
			}
			break
		}
		if _, err := validateRunTerminalSession(ctx, connection, run); err != nil {
			invalid = err
			break
		}
		if found {
			// Any terminal outcome may precede a retry: a send-back returns a
			// finished task, a success included, to its queue.
			if latest.Phase != RunTerminal {
				invalid = fmt.Errorf("%w: run history predecessor is not terminal", ErrCorruptState)
				break
			}
			if run.AdmittedAt.Int64() < latest.UpdatedAt.Int64() {
				invalid = fmt.Errorf("%w: run history admission predates predecessor terminal run", ErrCorruptState)
				break
			}
		}
		latest = run
		found = true
		expectedRevision++
	}
	rowsErr := rows.Err()
	closeErr := rows.Close()
	if rowsErr != nil {
		return rowsErr
	}
	if closeErr != nil {
		return closeErr
	}
	if invalid != nil {
		return invalid
	}
	if !found {
		// A queued task the console cancels before it is ever admitted has no
		// run history and never will.
		if (task.Status == TaskQueued || task.Status == TaskCancelled) && task.WorkRevision.Int64() == 1 {
			return nil
		}
		return fmt.Errorf("%w: task has no run history", ErrCorruptState)
	}
	delta := task.WorkRevision.Int64() - latest.AdmittedTaskWorkRevision.Int64()
	switch delta {
	case 0:
		if task.Status == TaskQueued || !taskMatchesRun(task, latest) {
			return fmt.Errorf("%w: current task does not match latest run", ErrCorruptState)
		}
	case 1:
		if latest.Phase != RunTerminal || task.Status != TaskQueued && task.Status != TaskCancelled {
			return fmt.Errorf("%w: queued task is not the next run revision", ErrCorruptState)
		}
		if task.UpdatedAt.Int64() < latest.UpdatedAt.Int64() {
			return fmt.Errorf("%w: queued retry predates predecessor terminal run", ErrCorruptState)
		}
	default:
		return fmt.Errorf("%w: task/run revisions are not contiguous", ErrCorruptState)
	}
	return nil
}

type ownershipLocator struct {
	path  string
	runID *RunID
}

func ownershipLocators(ctx context.Context, connection *sql.Conn) ([]ownershipLocator, error) {
	var locators []ownershipLocator
	resourceRows, err := connection.QueryContext(ctx, `SELECT run_id, path FROM resources WHERE kind = 'runtime_root'`)
	if err != nil {
		return nil, err
	}
	defer resourceRows.Close()
	for resourceRows.Next() {
		var rawRunID []byte
		var path string
		if err := resourceRows.Scan(&rawRunID, &path); err != nil {
			return nil, err
		}
		runID, err := RunIDFromBytes(rawRunID)
		if err != nil || !validOwnedLocator(path) {
			return nil, fmt.Errorf("%w: invalid runtime ownership locator", ErrCorruptState)
		}
		id := runID
		locators = append(locators, ownershipLocator{path: path, runID: &id})
	}
	if err := resourceRows.Err(); err != nil {
		return nil, err
	}
	return locators, nil
}

func validateOwnershipLocators(ctx context.Context, connection *sql.Conn) error {
	locators, err := ownershipLocators(ctx, connection)
	if err != nil {
		return err
	}
	return validateDistinctOwnershipLocators(locators)
}

func validateAdmissionLocatorOwnership(ctx context.Context, connection *sql.Conn, keys AdmissionKeys) error {
	locators, err := ownershipLocators(ctx, connection)
	if err != nil {
		return err
	}
	if err := validateDistinctOwnershipLocators(locators); err != nil {
		return err
	}
	for _, locator := range locators {
		if locator.runID != nil && *locator.runID == keys.RunID {
			continue
		}
		if pathsOverlap(locator.path, keys.RuntimeRoot) {
			return ErrConflict
		}
	}
	return nil
}

func validateDistinctOwnershipLocators(locators []ownershipLocator) error {
	paths := make(map[string]struct{}, len(locators))
	for _, locator := range locators {
		if _, duplicate := paths[locator.path]; duplicate {
			return fmt.Errorf("%w: durable ownership locators overlap", ErrCorruptState)
		}
		paths[locator.path] = struct{}{}
	}
	for path := range paths {
		for parent := filepath.Dir(path); parent != string(filepath.Separator); parent = filepath.Dir(parent) {
			if _, overlap := paths[parent]; overlap {
				return fmt.Errorf("%w: durable ownership locators overlap", ErrCorruptState)
			}
		}
	}
	return nil
}

func resourcesMatchRunPhase(phase RunPhase, resources []Resource) bool {
	if !exactResourceSet(resources, false) {
		return false
	}
	for _, resource := range resources {
		switch phase {
		case RunAdmitted:
			if resource.State != ResourceDeclared && resource.State != ResourceActive && (resource.Kind != ResourceRunnerProcess || resource.State != ResourceStarting) {
				return false
			}
		case RunRunning:
			if resource.State != ResourceActive {
				return false
			}
		case RunFinalizing:
			if resource.State != ResourceReleasing && resource.State != ResourceUnresolved && resource.State != ResourceReleased {
				return false
			}
		case RunTerminal:
			if resource.State != ResourceReleased {
				return false
			}
		default:
			return false
		}
	}
	return true
}

func validateProjects(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT id, name, root, verification_policy, revision, created_at_ms, updated_at_ms FROM projects`)
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		if _, _, err := scanProject(rows); err != nil {
			return err
		}
	}
	return rows.Err()
}

func validateAgents(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT `+agentColumns+` FROM agents`)
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		if _, _, err := scanAgent(rows); err != nil {
			return err
		}
	}
	return rows.Err()
}

func validateTasks(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT id, project_id, assigned_agent_id, incarnation_id, work_revision, title, body, sent_back_instruction_bytes, status, priority, blocked_reason, result, completed_at_ms, revision, created_at_ms, updated_at_ms FROM tasks`)
	if err != nil {
		return err
	}
	var tasks []Task
	var invalid error
	for rows.Next() {
		task, found, err := scanTask(rows)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			invalid = err
			break
		}
		tasks = append(tasks, task)
	}
	rowsErr := rows.Err()
	closeErr := rows.Close()
	if rowsErr != nil {
		return rowsErr
	}
	if closeErr != nil {
		return closeErr
	}
	if invalid != nil {
		return invalid
	}
	for _, task := range tasks {
		if err := validateTaskRunTopology(ctx, connection, task); err != nil {
			return err
		}
	}
	return nil
}

func validateChanges(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT `+changeColumns+` FROM changes`)
	if err != nil {
		return err
	}
	var changes []Change
	for rows.Next() {
		change, found, err := scanChange(rows)
		if err != nil || !found {
			rows.Close()
			if err == nil {
				err = ErrCorruptState
			}
			return err
		}
		changes = append(changes, change)
	}
	if err := errors.Join(rows.Err(), rows.Close()); err != nil {
		return err
	}
	for _, change := range changes {
		owner, ownerFound, err := scanRun(connection.QueryRowContext(ctx, `SELECT `+runColumns+` FROM runs WHERE change_id = ? AND phase <> 'terminal'`, change.ID.Bytes()))
		if err != nil {
			return err
		}
		if change.SettledRunID == nil {
			if !ownerFound {
				return fmt.Errorf("%w: unsettled Change has no current owner", ErrCorruptState)
			}
			ownership, ownershipErr := classifyWorkerChangeOwnership(ctx, connection, owner, change)
			if ownershipErr != nil {
				return fmt.Errorf("invalid unsettled Change authority: %w", ownershipErr)
			}
			if ownership.settled() {
				return fmt.Errorf("%w: invalid unsettled Change authority", ErrCorruptState)
			}
			continue
		}
		if ownerFound {
			return fmt.Errorf("%w: settled Change still has a current owner", ErrCorruptState)
		}
		run, found, err := runByID(ctx, connection, *change.SettledRunID)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return err
		}
		ownership, ownershipErr := classifyWorkerChangeOwnership(ctx, connection, run, change)
		if run.Phase != RunTerminal || ownershipErr != nil || !ownership.settled() || run.TerminalAt == nil || change.UpdatedAt.Int64() > run.TerminalAt.Int64() {
			return fmt.Errorf("%w: invalid Change settlement authority", ErrCorruptState)
		}
	}
	return nil
}

func validateRuns(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT `+runColumns+` FROM runs`)
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		if _, _, err := scanRun(rows); err != nil {
			return err
		}
	}
	return rows.Err()
}

func validateResources(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, `SELECT `+resourceColumns+` FROM resources`)
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		if _, _, err := scanResource(rows); err != nil {
			return err
		}
	}
	return rows.Err()
}

func validateInvalidations(ctx context.Context, connection *sql.Conn, factory FactoryState) error {
	if err := validateInvalidationBounds(ctx, connection, factory); err != nil {
		return err
	}
	if factory.Head.Int64() == 0 {
		return nil
	}
	rows, err := connection.QueryContext(ctx, `SELECT sequence, occurred_at_ms, entity_kind, entity_id, revision, deleted FROM invalidations ORDER BY sequence`)
	if err != nil {
		return err
	}
	defer rows.Close()
	expected := factory.Floor.Int64()
	for rows.Next() {
		var sequence, at, revision, deleted int64
		var kind string
		var id []byte
		if err := rows.Scan(&sequence, &at, &kind, &id, &revision, &deleted); err != nil {
			return err
		}
		entityKind, kindErr := parseEntityKind(kind)
		validID := len(id) == IDBytes
		if entityKind == EntityFactory {
			validID = validID && string(id) == string(factoryEntityID[:])
		} else {
			validID = validID && validNonzeroID(id)
		}
		if sequence != expected || at < 0 || revision < 1 || deleted != 0 && deleted != 1 || kindErr != nil || !validID {
			return fmt.Errorf("%w: invalid invalidation row", ErrCorruptState)
		}
		expected++
	}
	return rows.Err()
}

func validateInvalidationBounds(ctx context.Context, connection *sql.Conn, factory FactoryState) error {
	var count, minimum, maximum int64
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*), COALESCE(MIN(sequence), 0), COALESCE(MAX(sequence), 0) FROM invalidations`).Scan(&count, &minimum, &maximum); err != nil {
		return err
	}
	if count < 0 || count > EventRetentionLimit {
		return fmt.Errorf("%w: invalid invalidation count", ErrCorruptState)
	}
	if count == 0 {
		if factory.Head.Int64() != 0 || factory.Floor.Int64() != 1 {
			return fmt.Errorf("%w: empty invalidation log has advanced metadata", ErrCorruptState)
		}
		return nil
	}
	if minimum != factory.Floor.Int64() || maximum != factory.Head.Int64() || maximum-minimum+1 != count {
		return fmt.Errorf("%w: invalidation metadata or gap", ErrCorruptState)
	}
	return nil
}

func validNonzeroID(value []byte) bool {
	_, err := identifierFromBytes(value)
	return err == nil
}

func anyNullIntValid(values ...sql.NullInt64) bool {
	for _, value := range values {
		if value.Valid {
			return true
		}
	}
	return false
}
