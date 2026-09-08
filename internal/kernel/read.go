package kernel

import (
	"bytes"
	"context"
	"database/sql"
	"errors"
	"fmt"
	"math"
	"strings"
	"time"
)

type rowScanner interface {
	Scan(...any) error
}

// nullableBlob preserves SQL NULL separately from a present zero-length BLOB
// and refuses every non-BLOB SQLite storage class.
type nullableBlob struct {
	bytes []byte
	valid bool
}

func (value *nullableBlob) Scan(source any) error {
	value.bytes = nil
	value.valid = false
	switch source := source.(type) {
	case nil:
		return nil
	case []byte:
		value.bytes = bytes.Clone(source)
		value.valid = true
		return nil
	default:
		return fmt.Errorf("%w: nullable BLOB has an invalid SQLite storage class", ErrCorruptState)
	}
}

func projectByID(ctx context.Context, connection *sql.Conn, id ProjectID) (Project, bool, error) {
	if id.zero() {
		return Project{}, false, fmt.Errorf("%w: zero project identifier", ErrInvalidValue)
	}
	return scanProject(connection.QueryRowContext(ctx, `SELECT id, name, root, verification_policy, revision, created_at_ms, updated_at_ms FROM projects WHERE id = ?`, id.Bytes()))
}

func scanProject(scanner rowScanner) (Project, bool, error) {
	var rawID []byte
	var name, root, policyValue string
	var revision, createdAt, updatedAt int64
	if err := scanner.Scan(&rawID, &name, &root, &policyValue, &revision, &createdAt, &updatedAt); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return Project{}, false, nil
		}
		return Project{}, false, fmt.Errorf("scan project: %w", err)
	}
	id, err := ProjectIDFromBytes(rawID)
	policy, policyErr := parseVerificationPolicy(policyValue)
	if err != nil || policyErr != nil || byteLen(name) < 1 || byteLen(name) > 128 || !validAbsolutePath(root) || updatedAt < createdAt {
		return Project{}, false, fmt.Errorf("%w: invalid project row", ErrCorruptState)
	}
	rev, err := NewRevision(revision)
	if err != nil {
		return Project{}, false, fmt.Errorf("%w: invalid project revision", ErrCorruptState)
	}
	created, err := NewUnixMillis(createdAt)
	if err != nil {
		return Project{}, false, fmt.Errorf("%w: invalid project creation time", ErrCorruptState)
	}
	updated, err := NewUnixMillis(updatedAt)
	if err != nil {
		return Project{}, false, fmt.Errorf("%w: invalid project update time", ErrCorruptState)
	}
	return Project{ID: id, Name: name, Root: root, VerificationPolicy: policy, Revision: rev, CreatedAt: created, UpdatedAt: updated}, true, nil
}

func agentByID(ctx context.Context, connection *sql.Conn, id AgentID) (Agent, bool, error) {
	if id.zero() {
		return Agent{}, false, fmt.Errorf("%w: zero agent identifier", ErrInvalidValue)
	}
	return scanAgent(connection.QueryRowContext(ctx, `SELECT `+agentColumns+` FROM agents WHERE id = ?`, id.Bytes()))
}

const agentColumns = `id, project_id, name, role, provider, model, reasoning_effort, account_id, paused, tool_budget_limit, tool_calls_used, revision, created_at_ms, updated_at_ms, idle_policy, idle_after_seconds, idle_instruction, idle_run_budget, idle_runs_used`

func scanAgent(scanner rowScanner) (Agent, bool, error) {
	var rawID, rawProjectID, rawAccountID []byte
	var name, rawRole, rawProvider, rawIdlePolicy, idleInstruction string
	var model, effort sql.NullString
	var paused, budget, used, revision, createdAt, updatedAt, idleAfter, idleBudget, idleUsed int64
	if err := scanner.Scan(&rawID, &rawProjectID, &name, &rawRole, &rawProvider, &model, &effort, &rawAccountID, &paused, &budget, &used, &revision, &createdAt, &updatedAt, &rawIdlePolicy, &idleAfter, &idleInstruction, &idleBudget, &idleUsed); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return Agent{}, false, nil
		}
		return Agent{}, false, fmt.Errorf("scan agent: %w", err)
	}
	id, idErr := AgentIDFromBytes(rawID)
	projectID, projectErr := ProjectIDFromBytes(rawProjectID)
	role, roleErr := parseAgentRole(rawRole)
	provider, providerErr := ParseProvider(rawProvider)
	if idErr != nil || projectErr != nil || roleErr != nil || providerErr != nil || byteLen(name) < 1 || byteLen(name) > 128 || (paused != 0 && paused != 1) || budget < 1 || budget > 1_000_000_000 || used < 0 || used > budget || updatedAt < createdAt {
		return Agent{}, false, fmt.Errorf("%w: invalid agent row", ErrCorruptState)
	}
	if model.Valid && model.String == "" || effort.Valid && effort.String == "" || validateStoredProviderControls(provider, nullStringValue(model), nullStringValue(effort)) != nil {
		return Agent{}, false, fmt.Errorf("%w: invalid agent controls", ErrCorruptState)
	}
	idle, idleErr := idleRuleFromRow(rawIdlePolicy, idleAfter, idleInstruction, idleBudget, idleUsed)
	if idleErr != nil {
		return Agent{}, false, fmt.Errorf("%w: invalid agent idle rule", ErrCorruptState)
	}
	accountID, accountErr := optionalAccountID(rawAccountID)
	if accountErr != nil || provider == ProviderShell && !accountID.zero() {
		return Agent{}, false, fmt.Errorf("%w: invalid agent account", ErrCorruptState)
	}
	rev, revisionErr := NewRevision(revision)
	created, createdErr := NewUnixMillis(createdAt)
	updated, updatedErr := NewUnixMillis(updatedAt)
	if revisionErr != nil || createdErr != nil || updatedErr != nil {
		return Agent{}, false, fmt.Errorf("%w: invalid agent revision or time", ErrCorruptState)
	}
	return Agent{
		ID: id, ProjectID: projectID, Name: name, Role: role, Provider: provider,
		Model: nullStringValue(model), ReasoningEffort: nullStringValue(effort), AccountID: accountID, Idle: idle,
		Paused: paused == 1, ToolBudgetLimit: uint64(budget), ToolCallsUsed: uint64(used),
		Revision: rev, CreatedAt: created, UpdatedAt: updated,
	}, true, nil
}

// optionalAccountID reads the nullable agent account column. A NULL is the
// provider default, not a missing row.
func optionalAccountID(raw []byte) (AccountID, error) {
	if raw == nil {
		return AccountID{}, nil
	}
	return AccountIDFromBytes(raw)
}

const accountColumns = `id, provider, home, label, revision, created_at_ms, updated_at_ms`

func accountByID(ctx context.Context, connection *sql.Conn, id AccountID) (Account, bool, error) {
	if id.zero() {
		return Account{}, false, fmt.Errorf("%w: zero account identifier", ErrInvalidValue)
	}
	return scanAccount(connection.QueryRowContext(ctx, `SELECT `+accountColumns+` FROM accounts WHERE id = ?`, id.Bytes()))
}

func scanAccount(scanner rowScanner) (Account, bool, error) {
	var rawID []byte
	var rawProvider, home, label string
	var revision, createdAt, updatedAt int64
	if err := scanner.Scan(&rawID, &rawProvider, &home, &label, &revision, &createdAt, &updatedAt); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return Account{}, false, nil
		}
		return Account{}, false, fmt.Errorf("scan account: %w", err)
	}
	id, idErr := AccountIDFromBytes(rawID)
	provider, providerErr := ParseProvider(rawProvider)
	rev, revisionErr := NewRevision(revision)
	created, createdErr := NewUnixMillis(createdAt)
	updated, updatedErr := NewUnixMillis(updatedAt)
	if idErr != nil || providerErr != nil || revisionErr != nil || createdErr != nil || updatedErr != nil ||
		validateAccountFields(provider, home, label) != nil || updatedAt < createdAt {
		return Account{}, false, fmt.Errorf("%w: invalid account row", ErrCorruptState)
	}
	return Account{ID: id, Provider: provider, Home: home, Label: label, Revision: rev, CreatedAt: created, UpdatedAt: updated}, true, nil
}

func taskByID(ctx context.Context, connection *sql.Conn, id TaskID) (Task, bool, error) {
	if id.zero() {
		return Task{}, false, fmt.Errorf("%w: zero task identifier", ErrInvalidValue)
	}
	return scanTask(connection.QueryRowContext(ctx, `SELECT id, project_id, assigned_agent_id, incarnation_id, work_revision, title, body, sent_back_instruction_bytes, status, priority, blocked_reason, result, completed_at_ms, revision, created_at_ms, updated_at_ms FROM tasks WHERE id = ?`, id.Bytes()))
}

func scanTask(scanner rowScanner) (Task, bool, error) {
	var rawID, rawProjectID, rawAgentID, rawIncarnationID []byte
	var workRevision, priority, revision, createdAt, updatedAt int64
	var title, body, rawStatus string
	var sentBackInstructionBytes sql.NullInt64
	var blockedReasonText, resultText sql.NullString
	var completedAt sql.NullInt64
	if err := scanner.Scan(&rawID, &rawProjectID, &rawAgentID, &rawIncarnationID, &workRevision, &title, &body, &sentBackInstructionBytes, &rawStatus, &priority, &blockedReasonText, &resultText, &completedAt, &revision, &createdAt, &updatedAt); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return Task{}, false, nil
		}
		return Task{}, false, fmt.Errorf("scan task: %w", err)
	}
	id, idErr := TaskIDFromBytes(rawID)
	projectID, projectErr := ProjectIDFromBytes(rawProjectID)
	agentID, agentErr := AgentIDFromBytes(rawAgentID)
	incarnationID, incarnationErr := IncarnationIDFromBytes(rawIncarnationID)
	workRev, workRevisionErr := NewRevision(workRevision)
	status, statusErr := parseTaskStatus(rawStatus)
	rev, revisionErr := NewRevision(revision)
	created, createdErr := NewUnixMillis(createdAt)
	updated, updatedErr := NewUnixMillis(updatedAt)
	if idErr != nil || projectErr != nil || agentErr != nil || incarnationErr != nil || workRevisionErr != nil || statusErr != nil || revisionErr != nil || createdErr != nil || updatedErr != nil || byteLen(title) < 1 || byteLen(title) > 1024 || byteLen(body) > 131072 || priority < -1_000_000 || priority > 1_000_000 || updatedAt < createdAt {
		return Task{}, false, fmt.Errorf("%w: invalid task row", ErrCorruptState)
	}
	if sentBackInstructionBytes.Valid && (sentBackInstructionBytes.Int64 < 0 || sentBackInstructionBytes.Int64 > int64(byteLen(body)) || !strings.HasPrefix(body[sentBackInstructionBytes.Int64:], sentBackMarker)) {
		return Task{}, false, fmt.Errorf("%w: invalid sent-back instruction boundary", ErrCorruptState)
	}
	if blockedReasonText.Valid && (byteLen(blockedReasonText.String) < 1 || byteLen(blockedReasonText.String) > 4096) || resultText.Valid && byteLen(resultText.String) > 131072 {
		return Task{}, false, fmt.Errorf("%w: invalid task result fields", ErrCorruptState)
	}
	validState := false
	switch status {
	case TaskQueued, TaskRunning:
		validState = !blockedReasonText.Valid && !resultText.Valid && !completedAt.Valid
	case TaskBlocked:
		validState = blockedReasonText.Valid && !resultText.Valid && !completedAt.Valid
	case TaskSucceeded:
		validState = !blockedReasonText.Valid && completedAt.Valid
	case TaskFailed, TaskCancelled:
		validState = !blockedReasonText.Valid && !resultText.Valid && completedAt.Valid
	}
	if !validState || completedAt.Valid && (completedAt.Int64 < 0 || completedAt.Int64 != updatedAt) {
		return Task{}, false, fmt.Errorf("%w: inconsistent task state", ErrCorruptState)
	}
	var completed *UnixMillis
	if completedAt.Valid {
		value, err := NewUnixMillis(completedAt.Int64)
		if err != nil {
			return Task{}, false, fmt.Errorf("%w: invalid task completion time", ErrCorruptState)
		}
		completed = &value
	}
	var sentBack *int64
	if sentBackInstructionBytes.Valid {
		value := sentBackInstructionBytes.Int64
		sentBack = &value
	}
	return Task{
		ID: id, ProjectID: projectID, AssignedAgentID: agentID, IncarnationID: incarnationID,
		WorkRevision: workRev, Title: title, Body: body, SentBackInstructionBytes: sentBack, Status: status, Priority: priority,
		BlockedReason: nullStringValue(blockedReasonText), Result: nullStringValue(resultText), CompletedAt: completed,
		Revision: rev, CreatedAt: created, UpdatedAt: updated,
	}, true, nil
}

func factoryState(ctx context.Context, connection *sql.Conn) (FactoryState, error) {
	var rawDaemonID []byte
	var dispatch, capacity, revision, next, floor, updatedAt int64
	if err := connection.QueryRowContext(ctx, `SELECT daemon_id, dispatch_enabled, capacity, revision, next_invalidation_sequence, invalidation_floor, updated_at_ms FROM factory WHERE singleton = 1`).Scan(&rawDaemonID, &dispatch, &capacity, &revision, &next, &floor, &updatedAt); err != nil {
		return FactoryState{}, fmt.Errorf("read factory: %w", err)
	}
	daemonID, daemonErr := DaemonIDFromBytes(rawDaemonID)
	if daemonErr != nil || dispatch != 0 && dispatch != 1 || capacity < 1 || capacity > MaxFactoryCapacity || next < 1 || floor < 1 || floor > next || updatedAt < 0 {
		return FactoryState{}, fmt.Errorf("%w: invalid factory controls", ErrCorruptState)
	}
	rev, revErr := NewRevision(revision)
	head, headErr := NewEventSequence(next - 1)
	floorSequence, floorErr := NewEventSequence(floor)
	updated, updatedErr := NewUnixMillis(updatedAt)
	if daemonErr != nil || revErr != nil || headErr != nil || floorErr != nil || updatedErr != nil {
		return FactoryState{}, fmt.Errorf("%w: invalid factory revision or invalidation metadata", ErrCorruptState)
	}
	return FactoryState{DaemonID: daemonID, DispatchEnabled: dispatch == 1, Capacity: uint16(capacity), Revision: rev, Head: head, Floor: floorSequence, updatedAt: updated}, nil
}

func (store *Store) Factory(ctx context.Context) (FactoryState, error) {
	connection, err := store.readerConnection(ctx)
	if err != nil {
		return FactoryState{}, err
	}
	defer connection.Close()
	return factoryState(ctx, connection)
}

func (store *Store) Project(ctx context.Context, id ProjectID) (Project, bool, error) {
	connection, err := store.readerConnection(ctx)
	if err != nil {
		return Project{}, false, err
	}
	defer connection.Close()
	return projectByID(ctx, connection, id)
}

func (store *Store) Agent(ctx context.Context, id AgentID) (Agent, bool, error) {
	connection, err := store.readerConnection(ctx)
	if err != nil {
		return Agent{}, false, err
	}
	defer connection.Close()
	return agentByID(ctx, connection, id)
}

func (store *Store) Task(ctx context.Context, id TaskID) (Task, bool, error) {
	tx, err := store.beginRead(ctx)
	if err != nil {
		return Task{}, false, err
	}
	defer tx.Close()
	task, found, err := taskByID(ctx, tx.connection, id)
	if err != nil || !found {
		return task, found, err
	}
	if err := validateTaskRunTopology(ctx, tx.connection, task); err != nil {
		return Task{}, false, err
	}
	return task, true, nil
}

type readTx struct {
	connection *sql.Conn
	active     bool
}

func (store *Store) beginRead(ctx context.Context) (*readTx, error) {
	connection, err := store.readerConnection(ctx)
	if err != nil {
		return nil, err
	}
	return beginPinnedRead(ctx, connection)
}

func beginPinnedRead(ctx context.Context, connection *sql.Conn) (*readTx, error) {
	if _, err := connection.ExecContext(ctx, "BEGIN"); err != nil {
		releaseUncertainConnection(connection)
		return nil, fmt.Errorf("begin sqlite read: %w", err)
	}
	return &readTx{connection: connection, active: true}, nil
}

func (tx *readTx) Close() error {
	if !tx.active {
		return nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Duration(busyMilliseconds)*time.Millisecond)
	defer cancel()
	_, rollbackErr := tx.connection.ExecContext(ctx, "ROLLBACK")
	if rollbackErr != nil {
		releaseUncertainConnection(tx.connection)
	} else {
		rollbackErr = tx.connection.Close()
	}
	tx.active = false
	return rollbackErr
}

func (store *Store) Snapshot(ctx context.Context) (DashboardSnapshot, error) {
	tx, err := store.beginRead(ctx)
	if err != nil {
		return DashboardSnapshot{}, err
	}
	defer tx.Close()
	if err := validateDurableControls(ctx, tx.connection); err != nil {
		return DashboardSnapshot{}, err
	}
	state, err := factoryState(ctx, tx.connection)
	if err != nil {
		return DashboardSnapshot{}, err
	}
	var activeRuns int64
	if err := tx.connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM runs WHERE phase <> 'terminal'`).Scan(&activeRuns); err != nil {
		return DashboardSnapshot{}, fmt.Errorf("count active runs: %w", err)
	}
	if activeRuns < 0 || activeRuns > math.MaxUint16 {
		return DashboardSnapshot{}, fmt.Errorf("%w: invalid active run count", ErrCorruptState)
	}
	snapshot := DashboardSnapshot{
		Head:    state.Head,
		Factory: FactorySummary{DispatchEnabled: state.DispatchEnabled, Capacity: state.Capacity, ActiveRuns: uint16(activeRuns), Revision: state.Revision},
	}
	count := 0
	projectRows, err := tx.connection.QueryContext(ctx, `SELECT id, name, revision FROM projects ORDER BY id LIMIT ?`, SnapshotEntityLimit+1)
	if err != nil {
		return DashboardSnapshot{}, fmt.Errorf("read project summaries: %w", err)
	}
	for projectRows.Next() {
		var rawID []byte
		var name string
		var rawRevision int64
		if err := projectRows.Scan(&rawID, &name, &rawRevision); err != nil {
			projectRows.Close()
			return DashboardSnapshot{}, fmt.Errorf("scan project summary: %w", err)
		}
		id, idErr := ProjectIDFromBytes(rawID)
		revision, revisionErr := NewRevision(rawRevision)
		if idErr != nil || revisionErr != nil || byteLen(name) < 1 || byteLen(name) > 128 {
			projectRows.Close()
			return DashboardSnapshot{}, fmt.Errorf("%w: invalid project summary", ErrCorruptState)
		}
		count++
		if count > SnapshotEntityLimit {
			projectRows.Close()
			return DashboardSnapshot{}, ErrSnapshotTooLarge
		}
		snapshot.Projects = append(snapshot.Projects, ProjectSummary{ID: id, Name: name, Revision: revision})
	}
	if err := projectRows.Close(); err != nil {
		return DashboardSnapshot{}, err
	}
	agentRows, err := tx.connection.QueryContext(ctx, agentSummarySelect+` ORDER BY a.id LIMIT ?`, SnapshotEntityLimit+1)
	if err != nil {
		return DashboardSnapshot{}, fmt.Errorf("read agent summaries: %w", err)
	}
	for agentRows.Next() {
		summary, err := scanAgentSummary(agentRows)
		if err != nil {
			agentRows.Close()
			return DashboardSnapshot{}, fmt.Errorf("scan agent summary: %w", err)
		}
		count++
		if count > SnapshotEntityLimit {
			agentRows.Close()
			return DashboardSnapshot{}, ErrSnapshotTooLarge
		}
		snapshot.Agents = append(snapshot.Agents, summary)
	}
	if err := agentRows.Close(); err != nil {
		return DashboardSnapshot{}, err
	}
	taskRows, err := tx.connection.QueryContext(ctx, `SELECT id, project_id, assigned_agent_id, title, status, priority, revision FROM tasks ORDER BY priority DESC, created_at_ms ASC, id ASC LIMIT ?`, SnapshotEntityLimit+1)
	if err != nil {
		return DashboardSnapshot{}, fmt.Errorf("read task summaries: %w", err)
	}
	for taskRows.Next() {
		var rawID, rawProjectID, rawAgentID []byte
		var title, rawStatus string
		var priority, rawRevision int64
		if err := taskRows.Scan(&rawID, &rawProjectID, &rawAgentID, &title, &rawStatus, &priority, &rawRevision); err != nil {
			taskRows.Close()
			return DashboardSnapshot{}, fmt.Errorf("scan task summary: %w", err)
		}
		id, idErr := TaskIDFromBytes(rawID)
		projectID, projectErr := ProjectIDFromBytes(rawProjectID)
		agentID, agentErr := AgentIDFromBytes(rawAgentID)
		status, statusErr := parseTaskStatus(rawStatus)
		revision, revisionErr := NewRevision(rawRevision)
		if idErr != nil || projectErr != nil || agentErr != nil || statusErr != nil || revisionErr != nil || byteLen(title) < 1 || byteLen(title) > 1024 || priority < -1_000_000 || priority > 1_000_000 {
			taskRows.Close()
			return DashboardSnapshot{}, fmt.Errorf("%w: invalid task summary", ErrCorruptState)
		}
		count++
		if count > SnapshotEntityLimit {
			taskRows.Close()
			return DashboardSnapshot{}, ErrSnapshotTooLarge
		}
		snapshot.Tasks = append(snapshot.Tasks, TaskSummary{ID: id, ProjectID: projectID, AssignedAgentID: agentID, Title: title, Status: status.String(), Priority: priority, Revision: revision})
	}
	if err := taskRows.Close(); err != nil {
		return DashboardSnapshot{}, err
	}
	humanRequests, err := humanRequestProjections(ctx, tx.connection, SnapshotEntityLimit-count)
	if err != nil {
		return DashboardSnapshot{}, fmt.Errorf("read human request projections: %w", err)
	}
	snapshot.HumanRequests = humanRequests
	return snapshot, nil
}

func nullStringValue(value sql.NullString) string {
	if value.Valid {
		return value.String
	}
	return ""
}
