package kernel

import (
	"context"
	"database/sql"
	"fmt"
	"math"
)

// PublicStateEntityLimit bounds one complete public read. It is a fail-closed
// guard, not a page size: a Factory holding more entities than this returns
// ErrSnapshotTooLarge rather than a truncated projection.
const PublicStateEntityLimit = 4096

// PublicSnapshot is one transactionally pinned, complete public projection of
// the Factory. Every field is a positive allowlist: durable rows carry private
// columns (project roots, task bodies, agent budgets) that this projection
// cannot represent. Head is the durable invalidation head the snapshot was
// read at, and is the only value a client needs to watch for change.
type PublicSnapshot struct {
	Head          EventSequence
	Factory       FactorySummary
	Projects      []ProjectSummary
	Agents        []AgentSummary
	Tasks         []TaskSummary
	HumanRequests []HumanRequestProjection
	Accounts      []AccountSummary
}

// ReadPublicSnapshot reads one coherent public snapshot inside a single pinned
// SQLite read transaction, so no concurrent writer can produce a mixed-head
// result. It never truncates: exceeding the entity bound is a finite
// ErrSnapshotTooLarge failure.
func (store *Store) ReadPublicSnapshot(ctx context.Context) (PublicSnapshot, error) {
	tx, err := store.beginRead(ctx)
	if err != nil {
		return PublicSnapshot{}, err
	}
	defer tx.Close()
	if err := validateDurableControls(ctx, tx.connection); err != nil {
		return PublicSnapshot{}, err
	}
	state, err := factoryState(ctx, tx.connection)
	if err != nil {
		return PublicSnapshot{}, err
	}
	if err := enforcePublicStateCount(ctx, tx.connection); err != nil {
		return PublicSnapshot{}, err
	}
	var activeRuns int64
	if err := tx.connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM runs WHERE phase <> 'terminal'`).Scan(&activeRuns); err != nil {
		return PublicSnapshot{}, fmt.Errorf("count active runs: %w", err)
	}
	if activeRuns < 0 || activeRuns > math.MaxUint16 {
		return PublicSnapshot{}, fmt.Errorf("%w: invalid active run count", ErrCorruptState)
	}
	snapshot := PublicSnapshot{
		Head:    state.Head,
		Factory: FactorySummary{DispatchEnabled: state.DispatchEnabled, Capacity: state.Capacity, ActiveRuns: uint16(activeRuns), Revision: state.Revision},
	}
	if snapshot.Projects, err = readPublicProjects(ctx, tx.connection); err != nil {
		return PublicSnapshot{}, err
	}
	if snapshot.Agents, err = readPublicAgents(ctx, tx.connection); err != nil {
		return PublicSnapshot{}, err
	}
	if snapshot.Tasks, err = readPublicTasks(ctx, tx.connection); err != nil {
		return PublicSnapshot{}, err
	}
	if snapshot.HumanRequests, err = readPublicHumanRequests(ctx, tx.connection); err != nil {
		return PublicSnapshot{}, err
	}
	if snapshot.Accounts, err = readPublicAccounts(ctx, tx.connection); err != nil {
		return PublicSnapshot{}, err
	}
	if 1+len(snapshot.Projects)+len(snapshot.Agents)+len(snapshot.Tasks)+len(snapshot.HumanRequests)+len(snapshot.Accounts) > PublicStateEntityLimit {
		return PublicSnapshot{}, fmt.Errorf("%w: public snapshot rows disagree with their count", ErrCorruptState)
	}
	return snapshot, nil
}

// enforcePublicStateCount is the fail-closed bound. It runs inside the caller's
// pinned read so the counted rows are exactly the rows the snapshot returns.
func enforcePublicStateCount(ctx context.Context, connection *sql.Conn) error {
	var projects, agents, tasks, requests, accounts int64
	err := connection.QueryRowContext(ctx, `SELECT
        (SELECT COUNT(*) FROM projects),
        (SELECT COUNT(*) FROM agents),
        (SELECT COUNT(*) FROM tasks),
        (SELECT COUNT(*) FROM human_requests WHERE status IN ('open', 'delivering', 'delivery_unknown')),
        (SELECT COUNT(*) FROM accounts)`).Scan(&projects, &agents, &tasks, &requests, &accounts)
	if err != nil {
		return fmt.Errorf("count public state: %w", err)
	}
	if projects < 0 || agents < 0 || tasks < 0 || requests < 0 || accounts < 0 {
		return fmt.Errorf("%w: invalid public state count", ErrCorruptState)
	}
	if projects >= PublicStateEntityLimit || agents >= PublicStateEntityLimit || tasks >= PublicStateEntityLimit || requests >= PublicStateEntityLimit || accounts >= PublicStateEntityLimit {
		return ErrSnapshotTooLarge
	}
	if 1+projects+agents+tasks+requests+accounts > PublicStateEntityLimit {
		return ErrSnapshotTooLarge
	}
	return nil
}

// readPublicAccounts serves which logins are linked and where their
// configuration directories are. Tokens are never read here at all.
func readPublicAccounts(ctx context.Context, connection *sql.Conn) ([]AccountSummary, error) {
	accounts, err := readAccounts(ctx, connection)
	if err != nil {
		return nil, err
	}
	result := make([]AccountSummary, 0, len(accounts))
	for _, account := range accounts {
		result = append(result, AccountSummary{ID: account.ID, Provider: account.Provider.String(), Home: account.Home, Label: account.Label, Revision: account.Revision})
	}
	return result, nil
}

// Each read below selects only public columns. Private durable data is not
// loaded at all, so it cannot reach a projection by accident.

func readPublicProjects(ctx context.Context, connection *sql.Conn) ([]ProjectSummary, error) {
	rows, err := connection.QueryContext(ctx, `SELECT id, name, revision FROM projects ORDER BY id`)
	if err != nil {
		return nil, fmt.Errorf("read public projects: %w", err)
	}
	defer rows.Close()
	result := make([]ProjectSummary, 0)
	for rows.Next() {
		var rawID []byte
		var name string
		var rawRevision int64
		if err := rows.Scan(&rawID, &name, &rawRevision); err != nil {
			return nil, fmt.Errorf("scan public project: %w", err)
		}
		id, idErr := ProjectIDFromBytes(rawID)
		revision, revisionErr := NewRevision(rawRevision)
		if idErr != nil || revisionErr != nil || byteLen(name) < 1 || byteLen(name) > 128 {
			return nil, fmt.Errorf("%w: invalid public project", ErrCorruptState)
		}
		result = append(result, ProjectSummary{ID: id, Name: name, Revision: revision})
	}
	return result, rows.Err()
}

func readPublicAgents(ctx context.Context, connection *sql.Conn) ([]AgentSummary, error) {
	rows, err := connection.QueryContext(ctx, agentSummarySelect+` ORDER BY a.id`)
	if err != nil {
		return nil, fmt.Errorf("read public agents: %w", err)
	}
	defer rows.Close()
	result := make([]AgentSummary, 0)
	for rows.Next() {
		summary, err := scanAgentSummary(rows)
		if err != nil {
			return nil, err
		}
		result = append(result, summary)
	}
	return result, rows.Err()
}

func readPublicTasks(ctx context.Context, connection *sql.Conn) ([]TaskSummary, error) {
	rows, err := connection.QueryContext(ctx, `SELECT id, project_id, assigned_agent_id, title, status, priority, revision FROM tasks ORDER BY id`)
	if err != nil {
		return nil, fmt.Errorf("read public tasks: %w", err)
	}
	defer rows.Close()
	result := make([]TaskSummary, 0)
	for rows.Next() {
		var rawID, rawProjectID, rawAgentID []byte
		var title, rawStatus string
		var priority, rawRevision int64
		if err := rows.Scan(&rawID, &rawProjectID, &rawAgentID, &title, &rawStatus, &priority, &rawRevision); err != nil {
			return nil, fmt.Errorf("scan public task: %w", err)
		}
		id, idErr := TaskIDFromBytes(rawID)
		projectID, projectErr := ProjectIDFromBytes(rawProjectID)
		agentID, agentErr := AgentIDFromBytes(rawAgentID)
		status, statusErr := parseTaskStatus(rawStatus)
		revision, revisionErr := NewRevision(rawRevision)
		if idErr != nil || projectErr != nil || agentErr != nil || statusErr != nil || revisionErr != nil ||
			byteLen(title) < 1 || byteLen(title) > 1024 || priority < -1_000_000 || priority > 1_000_000 {
			return nil, fmt.Errorf("%w: invalid public task", ErrCorruptState)
		}
		result = append(result, TaskSummary{ID: id, ProjectID: projectID, AssignedAgentID: agentID, Title: title, Status: status.String(), Priority: priority, Revision: revision})
	}
	return result, rows.Err()
}

func readPublicHumanRequests(ctx context.Context, connection *sql.Conn) ([]HumanRequestProjection, error) {
	rows, err := connection.QueryContext(ctx, `SELECT id FROM human_requests WHERE status IN ('open', 'delivering', 'delivery_unknown') ORDER BY id`)
	if err != nil {
		return nil, fmt.Errorf("read public human requests: %w", err)
	}
	var ids []HumanRequestID
	for rows.Next() {
		var rawID []byte
		if err := rows.Scan(&rawID); err != nil {
			rows.Close()
			return nil, fmt.Errorf("scan public human request: %w", err)
		}
		id, err := HumanRequestIDFromBytes(rawID)
		if err != nil {
			rows.Close()
			return nil, fmt.Errorf("%w: invalid public human request identity", ErrCorruptState)
		}
		ids = append(ids, id)
	}
	if err := rows.Err(); err != nil {
		rows.Close()
		return nil, err
	}
	if err := rows.Close(); err != nil {
		return nil, err
	}
	result := make([]HumanRequestProjection, 0, len(ids))
	for _, id := range ids {
		projection, found, err := humanRequestProjectionByID(ctx, connection, id)
		if err != nil {
			return nil, err
		}
		if !found {
			return nil, fmt.Errorf("%w: public human request disappeared", ErrCorruptState)
		}
		result = append(result, projection)
	}
	return result, nil
}

// agentSummarySelect is the one derivation of the served agent summary.
// Provider is included as a public fact; live activity is deliberately
// absent (see AgentSummary).
const agentSummarySelect = `SELECT a.id, a.project_id, a.name, a.role, a.provider, a.paused, a.model, a.reasoning_effort, a.account_id, a.revision, a.idle_policy, a.idle_after_seconds, a.idle_instruction, a.idle_run_budget, a.idle_runs_used FROM agents a`

func scanAgentSummary(scanner rowScanner) (AgentSummary, error) {
	var rawID, rawProjectID, rawAccountID []byte
	var name, rawRole, rawProvider, rawIdlePolicy, idleInstruction string
	var model, effort sql.NullString
	var paused, rawRevision, idleAfter, idleBudget, idleUsed int64
	if err := scanner.Scan(&rawID, &rawProjectID, &name, &rawRole, &rawProvider, &paused, &model, &effort, &rawAccountID, &rawRevision, &rawIdlePolicy, &idleAfter, &idleInstruction, &idleBudget, &idleUsed); err != nil {
		return AgentSummary{}, err
	}
	idle, idleErr := idleRuleFromRow(rawIdlePolicy, idleAfter, idleInstruction, idleBudget, idleUsed)
	id, idErr := AgentIDFromBytes(rawID)
	accountID, accountErr := optionalAccountID(rawAccountID)
	projectID, projectErr := ProjectIDFromBytes(rawProjectID)
	role, roleErr := parseAgentRole(rawRole)
	provider, providerErr := ParseProvider(rawProvider)
	revision, revisionErr := NewRevision(rawRevision)
	if idErr != nil || projectErr != nil || roleErr != nil || providerErr != nil || revisionErr != nil || accountErr != nil || idleErr != nil ||
		byteLen(name) < 1 || byteLen(name) > 128 || paused != 0 && paused != 1 ||
		validateStoredProviderControls(provider, model.String, effort.String) != nil {
		return AgentSummary{}, fmt.Errorf("%w: invalid agent summary", ErrCorruptState)
	}
	return AgentSummary{ID: id, ProjectID: projectID, Name: name, Role: role.String(), Provider: provider.String(), Paused: paused == 1, Model: model.String, ReasoningEffort: effort.String, AccountID: accountID, Idle: idle, Revision: revision}, nil
}
