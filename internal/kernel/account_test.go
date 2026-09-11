package kernel

import (
	"bytes"
	"context"
	"errors"
	"path/filepath"
	"testing"
)

func accountID(t *testing.T, seed byte) AccountID {
	t.Helper()
	result, err := AccountIDFromBytes(bytes.Repeat([]byte{seed}, IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	return result
}

// An account is the provider's own login directory. Linking is idempotent on
// (provider, home) because that pair is the login's identity, and an agent may
// only select one that exists and matches its own provider.
func TestAccountLinkingAndAgentSelection(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	project, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 1), Name: "project", Root: filepath.Join(t.TempDir(), "root")}, mustTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	codex, err := store.LinkAccount(ctx, NewAccount{ID: accountID(t, 9), Provider: ProviderCodex, Home: "/Users/operator/.codex", Label: ".codex"}, mustTime(t, 11))
	if err != nil {
		t.Fatal(err)
	}
	if codex.Revision.Int64() != 1 || codex.Home != "/Users/operator/.codex" {
		t.Fatalf("linked account = %+v", codex)
	}
	// The same directory is the same login, whatever identity is offered.
	again, err := store.LinkAccount(ctx, NewAccount{ID: accountID(t, 10), Provider: ProviderCodex, Home: "/Users/operator/.codex", Label: "other"}, mustTime(t, 12))
	if err != nil || again.ID != codex.ID || again.Label != codex.Label {
		t.Fatalf("relink = %+v, %v", again, err)
	}
	claude, err := store.LinkAccount(ctx, NewAccount{ID: accountID(t, 11), Provider: ProviderClaudeCode, Home: "/Users/operator/.claude", Label: ".claude"}, mustTime(t, 13))
	if err != nil {
		t.Fatal(err)
	}
	accounts, err := store.ListAccounts(ctx)
	if err != nil || len(accounts) != 2 {
		t.Fatalf("accounts = %d, %v", len(accounts), err)
	}

	for _, refused := range []NewAccount{
		{ID: accountID(t, 12), Provider: ProviderShell, Home: "/Users/operator/.shell", Label: "shell"},
		{ID: accountID(t, 12), Provider: ProviderCodex, Home: "relative", Label: "codex"},
		{ID: accountID(t, 12), Provider: ProviderCodex, Home: "/", Label: "codex"},
		{ID: accountID(t, 12), Provider: ProviderCodex, Home: "/Users/operator/.codex2", Label: ""},
	} {
		if _, err := store.LinkAccount(ctx, refused, mustTime(t, 14)); !errors.Is(err, ErrInvalidValue) {
			t.Fatalf("account %+v accepted: %v", refused, err)
		}
	}

	// An agent selects one account of its own provider; anything else is refused.
	worker := NewAgent{ID: agentID(t, 2), ProjectID: project.ID, Name: "worker", Role: RoleWorker, Provider: ProviderCodex, AccountID: codex.ID, ToolBudgetLimit: 100}
	agent, err := store.CreateAgent(ctx, worker, mustTime(t, 15))
	if err != nil || agent.AccountID != codex.ID {
		t.Fatalf("agent = %+v, %v", agent, err)
	}
	for _, refused := range []NewAgent{
		{ID: agentID(t, 3), ProjectID: project.ID, Name: "crossed", Role: RoleWorker, Provider: ProviderCodex, AccountID: claude.ID, ToolBudgetLimit: 100},
		{ID: agentID(t, 4), ProjectID: project.ID, Name: "shell", Role: RoleWorker, Provider: ProviderShell, AccountID: codex.ID, ToolBudgetLimit: 100},
		{ID: agentID(t, 5), ProjectID: project.ID, Name: "absent", Role: RoleWorker, Provider: ProviderCodex, AccountID: accountID(t, 13), ToolBudgetLimit: 100},
	} {
		if _, err := store.CreateAgent(ctx, refused, mustTime(t, 16)); !errors.Is(err, ErrInvalidValue) {
			t.Fatalf("agent %q accepted a bad account: %v", refused.Name, err)
		}
	}

	// An agent with no account is the provider default, and stays valid.
	plain, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 6), ProjectID: project.ID, Name: "default", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 100}, mustTime(t, 17))
	if err != nil || (plain.AccountID != AccountID{}) {
		t.Fatalf("default agent = %+v, %v", plain, err)
	}

	// The console edit obeys the same rule, and an empty selection clears it.
	crossed := claude.ID
	if _, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{AccountID: &crossed}, mustTime(t, 18)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("crossed provider account accepted: %v", err)
	}
	cleared := AccountID{}
	updated, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{AccountID: &cleared}, mustTime(t, 19))
	if err != nil || (updated.AccountID != AccountID{}) {
		t.Fatalf("clear = %+v, %v", updated, err)
	}
	reselected, err := store.UpdateAgent(ctx, updated.ID, updated.Revision, AgentPatch{AccountID: &codex.ID}, mustTime(t, 20))
	if err != nil || reselected.AccountID != codex.ID {
		t.Fatalf("reselect = %+v, %v", reselected, err)
	}

	// Linked accounts are served, and every agent's selection with them.
	snapshot, err := store.ReadPublicSnapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.Accounts) != 2 || snapshot.Accounts[0].Home == "" || snapshot.Accounts[0].Revision.Int64() != 1 {
		t.Fatalf("served accounts = %+v", snapshot.Accounts)
	}
	selected := 0
	for _, summary := range snapshot.Agents {
		if summary.AccountID == codex.ID {
			selected++
		}
	}
	if selected != 1 {
		t.Fatalf("served agent account selections = %d, want 1", selected)
	}
}

func TestAccountUpdateRenamesAndUnlinksUnusedAccounts(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	account, err := store.LinkAccount(ctx, NewAccount{ID: accountID(t, 21), Provider: ProviderCodex, Home: "/Users/operator/.codex-update", Label: "old"}, mustTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	renamed, err := store.UpdateAccount(ctx, account.ID, account.Revision, stringPtr("new"), false, mustTime(t, 2))
	if err != nil || renamed.Label != "new" || renamed.Revision.Int64() != 2 {
		t.Fatalf("rename = %+v, %v", renamed, err)
	}
	if _, err := store.UpdateAccount(ctx, account.ID, account.Revision, stringPtr("stale"), false, mustTime(t, 3)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale rename = %v", err)
	}
	removed, err := store.UpdateAccount(ctx, account.ID, renamed.Revision, nil, true, mustTime(t, 4))
	if err != nil || removed.ID != account.ID || removed.Revision.Int64() != 3 {
		t.Fatalf("remove = %+v, %v", removed, err)
	}
	if accounts, err := store.ListAccounts(ctx); err != nil || len(accounts) != 0 {
		t.Fatalf("accounts after remove = %+v, %v", accounts, err)
	}
}

func TestAccountUpdateRefusesReferencedAccount(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	project, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 22), Name: "project", Root: filepath.Join(t.TempDir(), "root")}, mustTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	account, err := store.LinkAccount(ctx, NewAccount{ID: accountID(t, 23), Provider: ProviderCodex, Home: "/Users/operator/.codex-referenced", Label: "work"}, mustTime(t, 2))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 24), ProjectID: project.ID, Name: "worker", Role: RoleWorker, Provider: ProviderCodex, AccountID: account.ID, ToolBudgetLimit: 1}, mustTime(t, 3)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.UpdateAccount(ctx, account.ID, account.Revision, nil, true, mustTime(t, 4)); !errors.Is(err, ErrConflict) {
		t.Fatalf("referenced remove = %v", err)
	}
}

func stringPtr(value string) *string { return &value }
