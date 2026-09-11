package kernel

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"slices"
	"strings"
	"sync"
	"testing"
)

func humanQuestionClient(t *testing.T, store *Store, seed byte, capabilities BrowserCapabilityMask) BrowserClient {
	t.Helper()
	boot := browserTestBoot(t, seed)
	digest := mintBrowserChallenge(t, store, seed, boot, 1, 1000, capabilities)
	return pairBrowserClient(t, store, digest, boot, browserTestID(t, seed), browserKey(t), 2)
}

func humanKey(seed byte) [IDBytes]byte {
	var key [IDBytes]byte
	key[0] = seed
	key[IDBytes-1] = seed ^ 0xa5
	return key
}

func humanDeliveryID(t *testing.T, seed byte) HumanRequestDeliveryID {
	t.Helper()
	value := bytes.Repeat([]byte{seed}, IDBytes)
	value[IDBytes-1] = seed ^ 0x5a
	id, err := HumanRequestDeliveryIDFromBytes(value)
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func TestHumanQuestionCreationProjectionDetailAndIdempotency(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	before, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	client := humanQuestionClient(t, store, 201, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
	input := NewHumanQuestion{IdempotencyKey: humanKey(1), QuestionText: "QUESTION_PRIVATE_SENTINEL", Options: []string{"Continue with the narrow fix", "Stop this task"}}
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, input, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	replay, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, input, mustTime(t, 401))
	if err != nil || replay.ID != request.ID || replay.Revision != request.Revision {
		t.Fatalf("idempotent creation = %+v, %v", replay, err)
	}
	changed := input
	changed.QuestionText = "different"
	if _, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, changed, mustTime(t, 401)); !errors.Is(err, ErrConflict) {
		t.Fatalf("changed idempotency key content = %v", err)
	}
	changed = input
	changed.Options = []string{"Different"}
	if _, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, changed, mustTime(t, 401)); !errors.Is(err, ErrConflict) {
		t.Fatalf("changed options replay: %v", err)
	}
	projection, found, err := store.HumanRequest(ctx, request.ID)
	if err != nil || !found {
		t.Fatalf("projection = %+v, %v, found=%v", projection, err, found)
	}
	invalidations := invalidationsAfter(t, store, before.Head)
	seen := false
	for _, invalidation := range invalidations {
		if invalidation.EntityKind == EntityHumanRequest.String() && invalidation.EntityID == request.ID.String() && invalidation.Revision.Int64() == 1 {
			if invalidation.Deleted {
				t.Fatalf("open request emitted tombstone: %+v", invalidation)
			}
			seen = true
		}
	}
	if !seen {
		t.Fatalf("human request invalidation missing: %+v", invalidations)
	}
	if projection.Status != HumanRequestOpen || !projection.CanReply || projection.ProjectID != run.ProjectID || projection.AgentID != run.AgentID || projection.TaskID != run.TaskID {
		t.Fatalf("unsafe projection = %+v", projection)
	}
	encoded, err := json.Marshal(projection)
	if err != nil || (bytes.Contains(encoded, []byte(input.QuestionText)) || bytes.Contains(encoded, []byte(input.Options[0]))) {
		t.Fatalf("public projection leaked question: %s, %v", encoded, err)
	}
	detail, err := store.HumanRequestDetail(ctx, client.ID, request.ID, request.Revision)
	if err != nil || detail.QuestionText != input.QuestionText || !slices.Equal(detail.Options, input.Options) || detail.Revision != request.Revision {
		t.Fatalf("detail = %+v, %v", detail, err)
	}
	if snapshot, err := store.Snapshot(ctx); err != nil || len(snapshot.HumanRequests) != 1 {
		t.Fatalf("snapshot = %+v, %v", snapshot, err)
	} else if encoded, marshalErr := json.Marshal(snapshot); marshalErr != nil || (bytes.Contains(encoded, []byte(input.QuestionText)) || bytes.Contains(encoded, []byte(input.Options[0]))) {
		t.Fatalf("snapshot leaked question: %s, %v", encoded, marshalErr)
	}
	if _, err := store.HumanRequestDetail(ctx, humanQuestionClient(t, store, 202, BrowserCapabilityObserve).ID, request.ID, request.Revision); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("observation-only detail = %v", err)
	}
	if _, err := store.HumanRequestDetail(ctx, client.ID, request.ID, mustRevision(t, request.Revision.Int64()+1)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("wrong-revision detail = %v", err)
	}
}

func TestCancelHumanRequestRunAtomicallyResolvesRequestAndRevokesRun(t *testing.T) {
	ctx := context.Background()
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 240, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	terminalClient := humanQuestionClient(t, store, 239, BrowserCapabilityObserve|BrowserCapabilityTerminalInput)
	session, found, err := store.TerminalSession(ctx, keys.TerminalSessionID)
	if err != nil || !found {
		t.Fatalf("terminal session = %+v, found=%v, err=%v", session, found, err)
	}
	lease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, terminalClient.ID, run.Revision, session.Revision, mustTime(t, 300))
	if err != nil {
		t.Fatal(err)
	}
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(240), QuestionText: "cancel me"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	before, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for name, revisions := range map[string][2]Revision{
		"stale request": {mustRevision(t, request.Revision.Int64()+1), run.Revision},
		"stale run":     {request.Revision, mustRevision(t, run.Revision.Int64()+1)},
	} {
		t.Run(name, func(t *testing.T) {
			if _, _, err := store.CancelHumanRequestRun(ctx, client.ID, request.ID, revisions[0], revisions[1], mustTime(t, 401)); !errors.Is(err, ErrRevisionConflict) {
				t.Fatalf("cancellation = %v", err)
			}
		})
	}
	unchangedRun, _, err := store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	unchangedRequest, _, err := store.HumanRequest(ctx, request.ID)
	if err != nil {
		t.Fatal(err)
	}
	unchangedFactory, err := store.Factory(ctx)
	if err != nil || unchangedRun.Revision != run.Revision || unchangedRun.Phase != RunRunning || unchangedRequest.Revision != request.Revision || unchangedRequest.Status != HumanRequestOpen || unchangedFactory.Head != before.Head {
		t.Fatalf("stale cancellation changed state: run=%+v request=%+v factory=%+v err=%v", unchangedRun, unchangedRequest, unchangedFactory, err)
	}
	cancelled, resolved, err := store.CancelHumanRequestRun(ctx, client.ID, request.ID, request.Revision, run.Revision, mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	closedSession, found, sessionErr := store.TerminalSession(ctx, session.ID)
	if cancelled.ID != run.ID || cancelled.Revision.Int64() != run.Revision.Int64()+1 || cancelled.Phase != RunFinalizing || cancelled.CredentialRevokedAt == nil || cancelled.Proposal == nil || cancelled.Proposal.kind != OutcomeCancelled || resolved.ID != request.ID || resolved.Revision.Int64() != request.Revision.Int64()+1 || resolved.Status != HumanRequestResolved || resolved.Resolution == nil || *resolved.Resolution != HumanRequestResolutionCancelRun || !found || sessionErr != nil || closedSession.LeaseClientID != nil || closedSession.LeaseGeneration != lease.Generation+1 {
		t.Fatalf("cancel result = run=%+v request=%+v", cancelled, resolved)
	}
	assertHumanRequestInvalidation(t, store, before.Head, request.ID, resolved.Revision, true)
	if _, _, err := store.CancelHumanRequestRun(ctx, client.ID, request.ID, request.Revision, run.Revision, mustTime(t, 402)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("duplicate cancellation = %v", err)
	}
}

func TestHumanReplyAndCancellationRaceCommitsExactlyOneDecision(t *testing.T) {
	for iteration := 0; iteration < 10; iteration++ {
		store, run, _ := runningOrchestratorRun(t)
		client := humanQuestionClient(t, store, byte(220+iteration), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
		request, err := store.CreateHumanQuestionForAttempt(context.Background(), run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(byte(220 + iteration)), QuestionText: "choose once"}, mustTime(t, 400))
		if err != nil {
			store.Close()
			t.Fatal(err)
		}
		deliveryID := humanDeliveryID(t, byte(220+iteration))
		start := make(chan struct{})
		errorsSeen := make(chan error, 2)
		go func() {
			<-start
			_, err := store.BeginHumanReply(context.Background(), client.ID, request.ID, request.Revision, deliveryID, "reply", mustTime(t, 401))
			errorsSeen <- err
		}()
		go func() {
			<-start
			_, _, err := store.CancelHumanRequestRun(context.Background(), client.ID, request.ID, request.Revision, run.Revision, mustTime(t, 401))
			errorsSeen <- err
		}()
		close(start)
		first, second := <-errorsSeen, <-errorsSeen
		if (first == nil) == (second == nil) || first != nil && !errors.Is(first, ErrRevisionConflict) || second != nil && !errors.Is(second, ErrRevisionConflict) {
			store.Close()
			t.Fatalf("race errors = %v, %v", first, second)
		}
		projection, found, err := store.HumanRequest(context.Background(), request.ID)
		currentRun, runFound, runErr := store.Run(context.Background(), run.ID)
		if err != nil || runErr != nil || !found || !runFound {
			store.Close()
			t.Fatalf("race state read: request found=%v err=%v run found=%v err=%v", found, err, runFound, runErr)
		}
		if projection.Status == HumanRequestDelivering {
			if currentRun.Phase != RunRunning {
				store.Close()
				t.Fatalf("reply winner redirected run: request=%+v run=%+v", projection, currentRun)
			}
		} else if projection.Status == HumanRequestResolved {
			if currentRun.Phase != RunFinalizing {
				store.Close()
				t.Fatalf("cancel winner did not finalize origin: request=%+v run=%+v", projection, currentRun)
			}
		} else {
			store.Close()
			t.Fatalf("race request = %+v", projection)
		}
		store.Close()
	}
}

func TestHumanQuestionDeliveryRecoveryAndAckAreAtMostOnce(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 203, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(2), QuestionText: "question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	beforeDelivery, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	delivery, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, 4), "reply", mustTime(t, 401))
	if err != nil || delivery.Revision.Int64() != request.Revision.Int64()+1 || string(delivery.Reply) != "reply" {
		t.Fatalf("begin delivery = %+v, %v", delivery, err)
	}
	assertHumanRequestInvalidation(t, store, beforeDelivery.Head, request.ID, delivery.Revision, false)
	beforeResolution, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err := store.AcknowledgeHumanReply(ctx, request.ID, delivery.DeliveryID, delivery.Revision, mustTime(t, 402)); err != nil {
		t.Fatalf("acknowledge = %v", err)
	}
	assertHumanRequestInvalidation(t, store, beforeResolution.Head, request.ID, mustRevision(t, delivery.Revision.Int64()+1), true)
	if err := store.AcknowledgeHumanReply(ctx, request.ID, delivery.DeliveryID, delivery.Revision, mustTime(t, 403)); err != nil {
		t.Fatalf("duplicate acknowledge = %v", err)
	}
	projection, _, err := store.HumanRequest(ctx, request.ID)
	if err != nil || projection.Status != HumanRequestResolved {
		t.Fatalf("resolved projection = %+v, %v", projection, err)
	}
	if _, err := store.BeginHumanReply(ctx, client.ID, request.ID, delivery.Revision, humanDeliveryID(t, 5), "retry", mustTime(t, 404)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("resolved reply = %v", err)
	}
}

func TestHumanQuestionReplyAuthorityAndDeliveryCollision(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(14), QuestionText: "question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	observer := humanQuestionClient(t, store, 208, BrowserCapabilityObserve)
	if _, err := store.BeginHumanReply(ctx, observer.ID, request.ID, request.Revision, humanDeliveryID(t, 15), "reply", mustTime(t, 401)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("observation-only reply = %v", err)
	}
	client := humanQuestionClient(t, store, 209, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	deliveryID := humanDeliveryID(t, 16)
	delivery, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, deliveryID, "reply", mustTime(t, 402))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, deliveryID, "reply", mustTime(t, 403)); !errors.Is(err, ErrConflict) {
		t.Fatalf("delivery collision = %v", err)
	}
	if err := store.AcknowledgeHumanReply(ctx, request.ID, deliveryID, delivery.Revision, mustTime(t, 404)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.BeginHumanReply(ctx, client.ID, request.ID, delivery.Revision, humanDeliveryID(t, 17), "retry", mustTime(t, 405)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("resolved request reply = %v", err)
	}
}

func TestHumanQuestionRevokedClientCannotReply(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 210, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(18), QuestionText: "question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.RevokeBrowserClient(ctx, client.ID, client.Revision, mustTime(t, 401)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, 19), "reply", mustTime(t, 402)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("revoked client reply = %v", err)
	}
}

func TestHumanRequestDetailRequiresLivePrivateCapabilityAndExactRevision(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(120), QuestionText: "private detail"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	for name, capabilities := range map[string]BrowserCapabilityMask{
		"observe only":       BrowserCapabilityObserve,
		"human actions only": BrowserCapabilityObserve | BrowserCapabilityHumanActions,
	} {
		t.Run(name, func(t *testing.T) {
			client := humanQuestionClient(t, store, byte(121+len(name)), capabilities)
			if _, err := store.HumanRequestDetail(ctx, client.ID, request.ID, request.Revision); !errors.Is(err, ErrUnauthorized) {
				t.Fatalf("detail = %v", err)
			}
		})
	}
	private := humanQuestionClient(t, store, 150, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail)
	if _, err := store.HumanRequestDetail(ctx, private.ID, request.ID, Revision{}); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("zero revision detail = %v", err)
	}
	if _, err := store.HumanRequestDetail(ctx, private.ID, request.ID, mustRevision(t, request.Revision.Int64()+1)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("future revision detail = %v", err)
	}
	if detail, err := store.HumanRequestDetail(ctx, private.ID, request.ID, request.Revision); err != nil || detail.QuestionText != "private detail" {
		t.Fatalf("exact detail = %+v, %v", detail, err)
	}
	if _, err := store.RevokeBrowserClient(ctx, private.ID, private.Revision, mustTime(t, 401)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.HumanRequestDetail(ctx, private.ID, request.ID, request.Revision); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("revoked detail = %v", err)
	}
}

func TestHumanQuestionBoundAndRunUniquenessAreTransactional(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	var wg sync.WaitGroup
	results := make(chan error, 2)
	for seed := byte(20); seed < 22; seed++ {
		seed := seed
		wg.Add(1)
		go func() {
			defer wg.Done()
			_, err := store.createHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(seed), QuestionText: "question"}, mustTime(t, 400), 1)
			results <- err
		}()
	}
	wg.Wait()
	close(results)
	var successes, busy int
	for err := range results {
		switch {
		case err == nil:
			successes++
		case errors.Is(err, ErrBusy), errors.Is(err, ErrConflict):
			busy++
		default:
			t.Fatalf("concurrent bounded creation = %v", err)
		}
	}
	if successes != 1 || busy != 1 {
		t.Fatalf("concurrent bounded creation successes=%d busy=%d", successes, busy)
	}
	snapshot, err := store.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.HumanRequests) != 1 {
		t.Fatalf("bounded snapshot requests=%d", len(snapshot.HumanRequests))
	}
}

func TestHumanQuestionInvalidationFailureRollsBackRequestAndRun(t *testing.T) {
	ctx := context.Background()
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	requestInput := NewHumanQuestion{IdempotencyKey: humanKey(22), QuestionText: "question"}
	if _, err := store.writer.Exec(`CREATE TRIGGER reject_human_request_invalidation BEFORE INSERT ON invalidations WHEN NEW.entity_kind = 'human_request' BEGIN SELECT RAISE(ABORT, 'forced human request invalidation failure'); END`); err != nil {
		t.Fatal(err)
	}
	beforeFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, requestInput, mustTime(t, 400)); err == nil {
		t.Fatal("human request creation accepted failed invalidation")
	}
	if _, err := store.writer.Exec(`DROP TRIGGER reject_human_request_invalidation`); err != nil {
		t.Fatal(err)
	}
	var count int
	if err := store.writer.QueryRow(`SELECT COUNT(*) FROM human_requests`).Scan(&count); err != nil {
		t.Fatal(err)
	}
	afterFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if count != 0 || afterFactory.Head != beforeFactory.Head {
		t.Fatalf("failed creation left durable state: count=%d before=%d after=%d", count, beforeFactory.Head.Int64(), afterFactory.Head.Int64())
	}
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, requestInput, mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	proposal, err := NewBlockedProposal("waiting")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`CREATE TRIGGER reject_human_request_invalidation BEFORE INSERT ON invalidations WHEN NEW.entity_kind = 'human_request' BEGIN SELECT RAISE(ABORT, 'forced human request invalidation failure'); END`); err != nil {
		t.Fatal(err)
	}
	beforeRun, found, err := store.Run(ctx, run.ID)
	if err != nil || !found {
		t.Fatalf("run before failed finalizing = %+v, %v", beforeRun, err)
	}
	if _, err := store.ProposeAttemptOutcome(ctx, keys.AttemptDigest, proposal, mustTime(t, 402)); err == nil {
		t.Fatal("finalization accepted failed request invalidation")
	}
	if _, err := store.writer.Exec(`DROP TRIGGER reject_human_request_invalidation`); err != nil {
		t.Fatal(err)
	}
	afterRun, found, err := store.Run(ctx, run.ID)
	if err != nil || !found {
		t.Fatalf("run after failed finalizing = %+v, %v", afterRun, err)
	}
	if afterRun.Phase != RunRunning || afterRun.Revision != beforeRun.Revision {
		t.Fatalf("failed finalizing changed run: before=%+v after=%+v", beforeRun, afterRun)
	}
	current, _, err := store.HumanRequest(ctx, request.ID)
	if err != nil || current.Status != HumanRequestOpen {
		t.Fatalf("failed finalizing changed request: %+v, %v", current, err)
	}
}

func TestHumanQuestionResolutionTombstoneFailureRollsBackStatusAndHead(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 151, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(122), QuestionText: "question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	delivery, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, 123), "reply", mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	before, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`CREATE TRIGGER reject_resolution_tombstone BEFORE INSERT ON invalidations WHEN NEW.entity_kind = 'human_request' AND NEW.deleted = 1 BEGIN SELECT RAISE(ABORT, 'forced resolution tombstone failure'); END`); err != nil {
		t.Fatal(err)
	}
	if err := store.AcknowledgeHumanReply(ctx, request.ID, delivery.DeliveryID, delivery.Revision, mustTime(t, 402)); err == nil {
		t.Fatal("resolution accepted failed tombstone")
	}
	if _, err := store.writer.Exec(`DROP TRIGGER reject_resolution_tombstone`); err != nil {
		t.Fatal(err)
	}
	current, found, err := store.HumanRequest(ctx, request.ID)
	if err != nil || !found || current.Status != HumanRequestDelivering || current.Revision != delivery.Revision {
		t.Fatalf("failed resolution request = %+v, found=%v, err=%v", current, found, err)
	}
	after, err := store.Factory(ctx)
	if err != nil || after.Head != before.Head {
		t.Fatalf("failed resolution head = before %d after %d, err=%v", before.Head.Int64(), after.Head.Int64(), err)
	}
	if err := store.AcknowledgeHumanReply(ctx, request.ID, delivery.DeliveryID, delivery.Revision, mustTime(t, 403)); err != nil {
		t.Fatal(err)
	}
	assertHumanRequestInvalidation(t, store, before.Head, request.ID, mustRevision(t, delivery.Revision.Int64()+1), true)
}

func TestHumanQuestionCorruptChronologyFailsClosed(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	path := storePath(t, store)
	defer store.Close()
	client := humanQuestionClient(t, store, 211, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(23), QuestionText: "question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	delivery, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, 24), "reply", mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`PRAGMA ignore_check_constraints = ON`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`UPDATE human_requests SET delivery_started_at_ms = 399 WHERE id = ?`, request.ID.Bytes()); err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`PRAGMA ignore_check_constraints = OFF`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Snapshot(ctx); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("corrupt delivery chronology snapshot = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	if reopened, err := Open(ctx, path); !errors.Is(err, ErrCorruptState) {
		if reopened != nil {
			reopened.Close()
		}
		t.Fatalf("reopen corrupt delivery chronology = %v", err)
	}
	_ = delivery
}

func TestHumanQuestionUncertainDeliveryIsNotReplayedAndRestartRecovery(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	path := storePath(t, store)
	client := humanQuestionClient(t, store, 204, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(3), QuestionText: "question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	delivery, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, 6), "reply", mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	beforeRecovery, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if count, err := reopened.RecoverHumanDeliveries(ctx, mustTime(t, 402)); err != nil || count != 1 {
		t.Fatalf("recovery = count %d, err %v", count, err)
	}
	assertHumanRequestInvalidation(t, reopened, beforeRecovery.Head, request.ID, mustRevision(t, delivery.Revision.Int64()+1), false)
	if count, err := reopened.RecoverHumanDeliveries(ctx, mustTime(t, 403)); err != nil || count != 0 {
		t.Fatalf("idempotent recovery = count %d, err %v", count, err)
	}
	projection, _, err := reopened.HumanRequest(ctx, request.ID)
	if err != nil || projection.Status != HumanRequestDeliveryUnknown {
		t.Fatalf("unknown projection = %+v, %v", projection, err)
	}
	if err := reopened.AcknowledgeHumanReply(ctx, request.ID, delivery.DeliveryID, delivery.Revision, mustTime(t, 404)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("unknown acknowledgement = %v", err)
	}
}

func TestHumanQuestionFinalizingStalesOpenAndMakesDeliveryUnknown(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	_ = humanQuestionClient(t, store, 205, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	open, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(9), QuestionText: "open"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	proposal, err := NewBlockedProposal("waiting")
	if err != nil {
		t.Fatal(err)
	}
	beforeStale, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	finalizing, err := store.ProposeAttemptOutcome(ctx, run.CredentialDigest, proposal, mustTime(t, 401))
	if err != nil || finalizing.Phase != RunFinalizing {
		t.Fatalf("finalizing = %+v, %v", finalizing, err)
	}
	projection, _, err := store.HumanRequest(ctx, open.ID)
	if err != nil || projection.Status != HumanRequestStale {
		t.Fatalf("staled open request = %+v, %v", projection, err)
	}
	assertHumanRequestInvalidation(t, store, beforeStale.Head, open.ID, projection.Revision, true)

	// A second run proves that an already accepted external delivery is never
	// represented as a delivered answer merely because finalization started.
	store2, run2, _ := runningOrchestratorRun(t)
	defer store2.Close()
	client2 := humanQuestionClient(t, store2, 206, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	request, err := store2.CreateHumanQuestionForAttempt(ctx, run2.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(10), QuestionText: "pending"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	delivery, err := store2.BeginHumanReply(ctx, client2.ID, request.ID, request.Revision, humanDeliveryID(t, 11), "reply", mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store2.ProposeAttemptOutcome(ctx, run2.CredentialDigest, proposal, mustTime(t, 402)); err != nil {
		t.Fatal(err)
	}
	projection, _, err = store2.HumanRequest(ctx, request.ID)
	if err != nil || projection.Status != HumanRequestDeliveryUnknown {
		t.Fatalf("uncertain delivery after finalizing = %+v, %v", projection, err)
	}
	if err := store2.AcknowledgeHumanReply(ctx, request.ID, delivery.DeliveryID, delivery.Revision, mustTime(t, 403)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("ack after finalizing = %v", err)
	}
}

func TestHumanQuestionTerminalizationStalesResidualDeliveryExactlyOnce(t *testing.T) {
	ctx := context.Background()
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 207, BrowserCapabilityObserve|BrowserCapabilityHumanActions|BrowserCapabilityPrivateHumanRequestDetail)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(12), QuestionText: "terminal question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	delivery, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, 13), "reply", mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	finalizing, err := store.ProposeAttemptOutcome(ctx, keys.AttemptDigest, func() Proposal { p, _ := NewBlockedProposal("terminal"); return p }(), mustTime(t, 402))
	if err != nil || finalizing.Phase != RunFinalizing {
		t.Fatalf("finalizing = %+v, %v", finalizing, err)
	}
	unknown, _, err := store.HumanRequest(ctx, request.ID)
	if err != nil || unknown.Status != HumanRequestDeliveryUnknown {
		t.Fatalf("delivery after finalizing = %+v, %v", unknown, err)
	}
	observeMissingProcessExits(t, store, run.ID, 403)
	releaseAllRunResources(t, store, run.ID, 410)
	closed := closeTerminalSessionAtCurrent(t, store, run.ID, 415)
	if _, err := store.writer.Exec(`CREATE TRIGGER reject_terminal_human_invalidation BEFORE INSERT ON invalidations WHEN NEW.entity_kind = 'human_request' BEGIN SELECT RAISE(ABORT, 'forced terminal human request invalidation failure'); END`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.FinalizeRun(ctx, run.ID, closed.Revision, mustTime(t, 420)); err == nil {
		t.Fatal("terminalization accepted failed request invalidation")
	}
	if _, err := store.writer.Exec(`DROP TRIGGER reject_terminal_human_invalidation`); err != nil {
		t.Fatal(err)
	}
	unchanged, _, err := store.Run(ctx, run.ID)
	if err != nil || unchanged.Phase != RunFinalizing || unchanged.Revision != closed.Revision {
		t.Fatalf("failed terminalization changed run: %+v, %v", unchanged, err)
	}
	beforeTerminal, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	terminal, err := store.FinalizeRun(ctx, run.ID, closed.Revision, mustTime(t, 421))
	if err != nil || terminal.Phase != RunTerminal {
		t.Fatalf("terminal = %+v, %v", terminal, err)
	}
	stale, _, err := store.HumanRequest(ctx, request.ID)
	if err != nil || stale.Status != HumanRequestStale {
		t.Fatalf("terminal request = %+v, %v", stale, err)
	}
	assertHumanRequestInvalidation(t, store, beforeTerminal.Head, request.ID, stale.Revision, true)
	if _, err := store.HumanRequestDetail(ctx, client.ID, request.ID, request.Revision); !errors.Is(err, ErrConflict) {
		t.Fatalf("closed detail = %v", err)
	}
	snapshot, err := store.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.HumanRequests) != 0 {
		t.Fatalf("stale request remains in snapshot: %+v", snapshot.HumanRequests)
	}
	before, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.FinalizeRun(ctx, run.ID, terminal.Revision, mustTime(t, 422)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("duplicate terminalization = %v", err)
	}
	after, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if after.Head != before.Head {
		t.Fatalf("duplicate terminalization advanced invalidations: %d -> %d", before.Head.Int64(), after.Head.Int64())
	}
	if delivery.RunID != request.RunID {
		t.Fatal("delivery lost exact run binding")
	}
}

func TestHumanQuestionProcessExitConvergesRequestsAtomically(t *testing.T) {
	ctx := context.Background()
	tests := []struct {
		name       string
		provider   bool
		pending    bool
		wantStatus HumanRequestStatus
	}{
		{name: "provider/open", provider: true, wantStatus: HumanRequestStale},
		{name: "provider/delivering", provider: true, pending: true, wantStatus: HumanRequestDeliveryUnknown},
		{name: "runner/open", wantStatus: HumanRequestStale},
		{name: "runner/delivering", pending: true, wantStatus: HumanRequestDeliveryUnknown},
	}
	for index, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store, run, _ := runningOrchestratorRun(t)
			defer store.Close()
			request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{
				IdempotencyKey: humanKey(byte(40 + index)), QuestionText: "private process-exit question",
			}, mustTime(t, 400))
			if err != nil {
				t.Fatal(err)
			}
			if test.pending {
				client := humanQuestionClient(t, store, byte(60+index), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
				if _, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, byte(70+index)), "private reply", mustTime(t, 401)); err != nil {
					t.Fatal(err)
				}
			}
			before, err := store.Factory(ctx)
			if err != nil {
				t.Fatal(err)
			}
			exit, err := NewProcessExitCode(1, 7, mustTime(t, 402))
			if err != nil {
				t.Fatal(err)
			}
			var observed Run
			if test.provider {
				observed, err = store.ObserveProviderExit(ctx, run.ID, run.Revision, registeredProcessIdentity(t, store, run.ID, ResourceProviderProcess), exit, mustTime(t, 403))
			} else {
				// Runner disappearance from a running attempt finalizes through
				// the daemon failure edge; the owned absence edge then releases
				// the runner below.
				failure, _ := NewFailureProposal(FailureInternal, "runner exited before an attempt outcome")
				observed, err = store.FailRun(ctx, run.ID, run.Revision, failure, mustTime(t, 403))
			}
			if err != nil || observed.Phase != RunFinalizing {
				t.Fatalf("process exit = %+v, %v", observed, err)
			}
			projection, found, err := store.HumanRequest(ctx, request.ID)
			if err != nil || !found || projection.Status != test.wantStatus {
				t.Fatalf("request after process exit = %+v, found=%v, err=%v", projection, found, err)
			}
			if _, err := store.Snapshot(ctx); err != nil {
				t.Fatalf("snapshot after process exit = %v", err)
			}
			invalidations := invalidationsAfter(t, store, before.Head)
			var runEvents, requestEvents int
			for _, invalidation := range invalidations {
				switch {
				case invalidation.EntityKind == EntityRun.String() && invalidation.EntityID == run.ID.String() && invalidation.Revision == observed.Revision:
					runEvents++
				case invalidation.EntityKind == EntityHumanRequest.String() && invalidation.EntityID == request.ID.String() && invalidation.Revision == projection.Revision:
					if invalidation.Deleted != (test.wantStatus == HumanRequestStale) {
						t.Fatalf("process-exit tombstone = %+v, want deleted=%v", invalidation, test.wantStatus == HumanRequestStale)
					}
					requestEvents++
				}
			}
			if runEvents != 1 || requestEvents != 1 {
				t.Fatalf("process-exit invalidations run=%d request=%d: %+v", runEvents, requestEvents, invalidations)
			}

			// The other owner may report later; the request transition must not
			// wedge finalization or route anything to a future retry. When the
			// provider exited first, the runner reports through its owned live
			// exit/release edge inside releaseAllRunResources.
			if !test.provider {
				otherExit, _ := NewProcessExitCode(2, 0, mustTime(t, 404))
				observed, err = store.ObserveProviderExit(ctx, run.ID, observed.Revision, registeredProcessIdentity(t, store, run.ID, ResourceProviderProcess), otherExit, mustTime(t, 405))
				if err != nil {
					t.Fatal(err)
				}
			}
			releaseAllRunResources(t, store, run.ID, 410)
			closed := closeTerminalSessionAtCurrent(t, store, run.ID, 420)
			terminal, err := store.FinalizeRun(ctx, run.ID, closed.Revision, mustTime(t, 430))
			if err != nil || terminal.Phase != RunTerminal {
				t.Fatalf("terminal after process exit = %+v, %v", terminal, err)
			}
			finalRequest, _, err := store.HumanRequest(ctx, request.ID)
			if err != nil || finalRequest.Status != HumanRequestStale {
				t.Fatalf("terminal request = %+v, %v", finalRequest, err)
			}
		})
	}
}

func assertHumanRequestInvalidation(t *testing.T, store *Store, after EventSequence, id HumanRequestID, revision Revision, deleted bool) {
	t.Helper()
	invalidations := invalidationsAfter(t, store, after)
	for _, invalidation := range invalidations {
		if invalidation.EntityKind == EntityHumanRequest.String() && invalidation.EntityID == id.String() && invalidation.Revision == revision {
			if invalidation.Deleted != deleted {
				t.Fatalf("human request invalidation = %+v, want deleted=%v", invalidation, deleted)
			}
			return
		}
	}
	t.Fatalf("human request invalidation id=%s revision=%d missing from %+v", id.String(), revision.Int64(), invalidations)
}

func TestHumanQuestionProcessExitInvalidationFailureRollsBackBothTransitions(t *testing.T) {
	ctx := context.Background()
	// The provider exit is the only remaining direct process-exit edge.
	for _, name := range []string{"provider"} {
		t.Run(name, func(t *testing.T) {
			store, run, _ := runningOrchestratorRun(t)
			defer store.Close()
			request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(byte(90 + len(name))), QuestionText: "question"}, mustTime(t, 400))
			if err != nil {
				t.Fatal(err)
			}
			if _, err := store.writer.Exec(`CREATE TRIGGER reject_process_exit_human_invalidation BEFORE INSERT ON invalidations WHEN NEW.entity_kind = 'human_request' BEGIN SELECT RAISE(ABORT, 'forced process-exit human invalidation failure'); END`); err != nil {
				t.Fatal(err)
			}
			beforeFactory, err := store.Factory(ctx)
			if err != nil {
				t.Fatal(err)
			}
			exit, _ := NewProcessExitCode(1, 7, mustTime(t, 402))
			_, observeErr := store.ObserveProviderExit(ctx, run.ID, run.Revision, registeredProcessIdentity(t, store, run.ID, ResourceProviderProcess), exit, mustTime(t, 403))
			if observeErr == nil {
				t.Fatal("process exit accepted failed request invalidation")
			}
			if _, err := store.writer.Exec(`DROP TRIGGER reject_process_exit_human_invalidation`); err != nil {
				t.Fatal(err)
			}
			unchanged, found, err := store.Run(ctx, run.ID)
			if err != nil || !found || unchanged.Phase != RunRunning || unchanged.Revision != run.Revision {
				t.Fatalf("failed process exit changed run = %+v, found=%v, err=%v", unchanged, found, err)
			}
			unchangedRequest, _, err := store.HumanRequest(ctx, request.ID)
			if err != nil || unchangedRequest.Status != HumanRequestOpen || unchangedRequest.Revision != request.Revision {
				t.Fatalf("failed process exit changed request = %+v, err=%v", unchangedRequest, err)
			}
			afterFactory, err := store.Factory(ctx)
			if err != nil || afterFactory.Head != beforeFactory.Head {
				t.Fatalf("failed process exit changed invalidations = before=%d after=%d err=%v", beforeFactory.Head.Int64(), afterFactory.Head.Int64(), err)
			}
		})
	}
}

func TestHumanQuestionImpossibleAdmittedRunPhasesFailClosed(t *testing.T) {
	ctx := context.Background()
	for _, status := range []string{"delivery_unknown", "resolved"} {
		t.Run(status, func(t *testing.T) {
			store, run, _ := admittedOrchestratorRun(t)
			defer store.Close()
			requestID := bytes.Repeat([]byte{0x91}, IDBytes)
			key := bytes.Repeat([]byte{0x92}, IDBytes)
			delivery := bytes.Repeat([]byte{0x93}, IDBytes)
			var deliveryID, resolution, closed any
			if status == "delivery_unknown" {
				deliveryID, resolution, closed = delivery, nil, nil
			} else {
				deliveryID, resolution, closed = delivery, "reply", int64(12)
			}
			_, err := store.writer.Exec(`INSERT INTO human_requests(id, run_id, idempotency_key, kind, reason_code, question_text, status, delivery_id, delivery_started_at_ms, resolution_kind, closed_at_ms, revision, created_at_ms, updated_at_ms) VALUES(?, ?, ?, 'question', 'provider_question', 'private question', ?, ?, ?, ?, ?, ?, ?, ?)`, requestID, run.ID.Bytes(), key, status, deliveryID, 11, resolution, closed, 2, 11, 12)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := store.Snapshot(ctx); !errors.Is(err, ErrCorruptState) {
				t.Fatalf("%s request attached to admitted run = %v", status, err)
			}
		})
	}
}

func TestHumanQuestionCreationRequiresExactRunningAttempt(t *testing.T) {
	ctx := context.Background()
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	input := NewHumanQuestion{IdempotencyKey: humanKey(7), QuestionText: "question"}
	for _, digest := range []AttemptDigest{keys.AttemptDigest, func() AttemptDigest {
		value, _ := AttemptDigestFromBytes(bytes.Repeat([]byte{0xfe}, DigestBytes))
		return value
	}()} {
		if digest == keys.AttemptDigest {
			continue
		}
		if _, err := store.CreateHumanQuestionForAttempt(ctx, digest, input, mustTime(t, 400)); !errors.Is(err, ErrUnauthorized) {
			t.Fatalf("forged attempt = %v", err)
		}
	}
	if _, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, input, mustTime(t, run.UpdatedAt.Int64()-1)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale timestamp = %v", err)
	}
	if _, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(8), QuestionText: strings.Repeat("x", MaxHumanRequestQuestionBytes+1)}, mustTime(t, 400)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("oversized question = %v", err)
	}
}
