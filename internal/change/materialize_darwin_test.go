//go:build darwin

package change

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"runtime/debug"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

var injectedFailure = errors.New("injected Change failure")

type changeFixture struct {
	manifest Manifest
	blobs    map[string][]byte
}

func TestPrepareRetainsDeclaredEmptyStageBeforeExplicitPopulate(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("a")}})
	sourceCalled := false
	prepared, err := Prepare(context.Background(), parent, "target", "declared-stage")
	if err != nil {
		t.Fatal(err)
	}
	identity := prepared.Identity()
	if identity == (StageIdentity{}) {
		t.Fatal("Prepare returned an empty identity")
	}
	assertEmptyExactDirectory(t, filepath.Join(parent, "declared-stage"), identity)
	if _, err := os.Lstat(filepath.Join(parent, "target")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("Prepare published target: %v", err)
	}
	if sourceCalled {
		t.Fatal("Prepare read a blob")
	}

	published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, func(ctx context.Context, oid ObjectID) ([]byte, error) {
		sourceCalled = true
		return fixture.source(ctx, oid)
	})
	if err != nil {
		t.Fatal(err)
	}
	if !sourceCalled || !published.Facts().Identity().Equal(identity) || published.Path() != filepath.Join(parent, "target") {
		t.Fatalf("unexpected publication: source=%v path=%q facts=%+v", sourceCalled, published.Path(), published.Facts())
	}
	if _, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source); !isLifecycle(err) {
		t.Fatalf("repeat Populate = %T %v, want LifecycleError", err, err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); !isLifecycle(err) {
		t.Fatalf("repeat Close = %T %v, want LifecycleError", err, err)
	}
	assertExactTree(t, published.Path(), fixture)

	closed, err := Prepare(context.Background(), parent, "never-target", "closed-stage")
	if err != nil {
		t.Fatal(err)
	}
	closedID := closed.Identity()
	if err := closed.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := closed.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source); !isLifecycle(err) {
		t.Fatalf("Populate after Close = %T %v, want LifecycleError", err, err)
	}
	removeRecorded(t, parent, "closed-stage", closedID)
}

func TestAdoptPreparedRetainsExactIdentityAndUsesExistingLifecycle(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("a")}})
	original := mustPrepare(t, parent, "change", "crash-stage")
	wantIdentity := original.Identity()
	if err := original.Close(); err != nil {
		t.Fatal(err)
	}

	adopted, err := AdoptPrepared(context.Background(), parent, "change", "crash-stage")
	if err != nil {
		t.Fatal(err)
	}
	if !adopted.Identity().Equal(wantIdentity) {
		t.Fatalf("adopted identity=%+v want=%+v", adopted.Identity(), wantIdentity)
	}
	published, err := adopted.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
	if err != nil {
		t.Fatal(err)
	}
	if err := adopted.Close(); err != nil {
		t.Fatal(err)
	}
	if !published.Facts().Identity().Equal(wantIdentity) {
		t.Fatalf("published identity=%+v want=%+v", published.Facts().Identity(), wantIdentity)
	}
	assertExactTree(t, published.Path(), fixture)
}

func TestAdoptPreparedRequiresAbsentTargetAndEmptyPrivateStageWithoutMutation(t *testing.T) {
	t.Run("missing stage", func(t *testing.T) {
		parent := secureTempDir(t)
		if _, err := AdoptPrepared(context.Background(), parent, "target", "stage"); err == nil {
			t.Fatal("missing stage adopted")
		}
		assertDirectoryEmpty(t, parent)
	})

	t.Run("existing target", func(t *testing.T) {
		parent := secureTempDir(t)
		prepared := mustPrepare(t, parent, "target", "stage")
		identity := prepared.Identity()
		if err := prepared.Close(); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(parent, "target"), []byte("foreign"), 0o600); err != nil {
			t.Fatal(err)
		}
		if _, err := AdoptPrepared(context.Background(), parent, "target", "stage"); err == nil {
			t.Fatal("stage adopted despite existing target")
		}
		assertEmptyExactDirectory(t, filepath.Join(parent, "stage"), identity)
		if data, err := os.ReadFile(filepath.Join(parent, "target")); err != nil || string(data) != "foreign" {
			t.Fatalf("target changed: %q %v", data, err)
		}
	})

	t.Run("target appears after stage open", func(t *testing.T) {
		parent := secureTempDir(t)
		prepared := mustPrepare(t, parent, "target", "stage")
		identity := prepared.Identity()
		if err := prepared.Close(); err != nil {
			t.Fatal(err)
		}
		hook := func(point materializePoint) error {
			if point.step == stepAfterAdoptOpen {
				return os.WriteFile(filepath.Join(parent, "target"), []byte("foreign"), 0o600)
			}
			return nil
		}
		if adopted, err := adoptPrepared(context.Background(), parent, "target", "stage", hook); err == nil {
			_ = adopted.Close()
			t.Fatal("stage adopted after target appeared")
		}
		assertEmptyExactDirectory(t, filepath.Join(parent, "stage"), identity)
		if data, err := os.ReadFile(filepath.Join(parent, "target")); err != nil || string(data) != "foreign" {
			t.Fatalf("target changed: %q %v", data, err)
		}
	})

	mutations := []struct {
		name   string
		create func(testing.TB, string)
		check  func(testing.TB, string)
	}{
		{
			name: "wrong mode",
			create: func(t testing.TB, path string) {
				t.Helper()
				if err := os.Mkdir(path, 0o755); err != nil {
					t.Fatal(err)
				}
				// Mkdir is umask-masked; under a restrictive umask the stage
				// would land 0700 — the accepted mode — and the wrong-mode
				// refusal this case exists to prove would never be exercised.
				if err := os.Chmod(path, 0o755); err != nil {
					t.Fatal(err)
				}
			},
			check: func(t testing.TB, path string) {
				t.Helper()
				if info, err := os.Lstat(path); err != nil || !info.IsDir() || info.Mode().Perm() != 0o755 {
					t.Fatalf("wrong-mode stage changed: %v %v", info, err)
				}
			},
		},
		{
			name: "regular file",
			create: func(t testing.TB, path string) {
				t.Helper()
				if err := os.WriteFile(path, []byte("retained"), 0o600); err != nil {
					t.Fatal(err)
				}
			},
			check: func(t testing.TB, path string) {
				t.Helper()
				if data, err := os.ReadFile(path); err != nil || string(data) != "retained" {
					t.Fatalf("regular stage changed: %q %v", data, err)
				}
			},
		},
		{
			name: "symlink",
			create: func(t testing.TB, path string) {
				t.Helper()
				if err := os.Symlink("elsewhere", path); err != nil {
					t.Fatal(err)
				}
			},
			check: func(t testing.TB, path string) {
				t.Helper()
				if target, err := os.Readlink(path); err != nil || target != "elsewhere" {
					t.Fatalf("symlink stage changed: %q %v", target, err)
				}
			},
		},
		{
			name: "fifo",
			create: func(t testing.TB, path string) {
				t.Helper()
				if err := unix.Mkfifo(path, 0o600); err != nil {
					t.Fatal(err)
				}
			},
			check: func(t testing.TB, path string) {
				t.Helper()
				if info, err := os.Lstat(path); err != nil || info.Mode()&os.ModeNamedPipe == 0 {
					t.Fatalf("fifo stage changed: %v %v", info, err)
				}
			},
		},
	}
	for _, mutation := range mutations {
		t.Run(mutation.name, func(t *testing.T) {
			parent := secureTempDir(t)
			stage := filepath.Join(parent, "stage")
			mutation.create(t, stage)
			if _, err := AdoptPrepared(context.Background(), parent, "target", "stage"); err == nil {
				t.Fatal("unsafe stage adopted")
			}
			mutation.check(t, stage)
		})
	}

	contents := []struct {
		name string
		add  func(testing.TB, string)
	}{
		{"file", func(t testing.TB, stage string) {
			if err := os.WriteFile(filepath.Join(stage, "partial"), []byte("retained"), 0o600); err != nil {
				t.Fatal(err)
			}
		}},
		{"hidden partial", func(t testing.TB, stage string) {
			if err := os.WriteFile(filepath.Join(stage, ".partial"), []byte("retained"), 0o600); err != nil {
				t.Fatal(err)
			}
		}},
		{"directory", func(t testing.TB, stage string) {
			if err := os.Mkdir(filepath.Join(stage, "nested"), 0o700); err != nil {
				t.Fatal(err)
			}
		}},
		{"symlink entry", func(t testing.TB, stage string) {
			if err := os.Symlink("outside", filepath.Join(stage, "partial")); err != nil {
				t.Fatal(err)
			}
		}},
		{"deep", func(t testing.TB, stage string) {
			path := stage
			for i := 0; i < 20; i++ {
				path = filepath.Join(path, fmt.Sprintf("d-%d", i))
				if err := os.Mkdir(path, 0o700); err != nil {
					t.Fatal(err)
				}
			}
		}},
		{"wide", func(t testing.TB, stage string) {
			for i := 0; i < 100; i++ {
				if err := os.WriteFile(filepath.Join(stage, fmt.Sprintf("f-%d", i)), nil, 0o600); err != nil {
					t.Fatal(err)
				}
			}
		}},
	}
	for _, content := range contents {
		t.Run("nonempty "+content.name, func(t *testing.T) {
			parent := secureTempDir(t)
			prepared := mustPrepare(t, parent, "target", "stage")
			if err := prepared.Close(); err != nil {
				t.Fatal(err)
			}
			stage := filepath.Join(parent, "stage")
			content.add(t, stage)
			if _, err := AdoptPrepared(context.Background(), parent, "target", "stage"); err == nil {
				t.Fatal("nonempty stage adopted")
			}
			entries, err := os.ReadDir(stage)
			if err != nil || len(entries) == 0 {
				t.Fatalf("refusal removed stage contents: %v %v", entries, err)
			}
		})
	}
}

func TestAdoptPreparedRejectsStageAndParentSwapsAtOpenBoundaries(t *testing.T) {
	for _, step := range []materializeStep{stepBeforeAdoptOpen, stepAfterAdoptOpen} {
		t.Run("stage "+string(step), func(t *testing.T) {
			parent := secureTempDir(t)
			prepared := mustPrepare(t, parent, "target", "stage")
			identity := prepared.Identity()
			if err := prepared.Close(); err != nil {
				t.Fatal(err)
			}
			hook := func(point materializePoint) error {
				if point.step != step {
					return nil
				}
				if err := os.Rename(filepath.Join(parent, "stage"), filepath.Join(parent, "held-stage")); err != nil {
					return err
				}
				return os.Mkdir(filepath.Join(parent, "stage"), 0o700)
			}
			if adopted, err := adoptPrepared(context.Background(), parent, "target", "stage", hook); err == nil {
				_ = adopted.Close()
				t.Fatal("stage swap adopted")
			}
			assertEmptyExactDirectory(t, filepath.Join(parent, "held-stage"), identity)
			if info, err := os.Lstat(filepath.Join(parent, "stage")); err != nil || !info.IsDir() {
				t.Fatalf("replacement stage not retained: %v %v", info, err)
			}
		})

		t.Run("parent "+string(step), func(t *testing.T) {
			root := secureTempDir(t)
			parent := filepath.Join(root, "parent")
			if err := os.Mkdir(parent, 0o700); err != nil {
				t.Fatal(err)
			}
			prepared := mustPrepare(t, parent, "target", "stage")
			identity := prepared.Identity()
			if err := prepared.Close(); err != nil {
				t.Fatal(err)
			}
			hook := func(point materializePoint) error {
				if point.step != step {
					return nil
				}
				if err := os.Rename(parent, filepath.Join(root, "held-parent")); err != nil {
					return err
				}
				return os.Mkdir(parent, 0o700)
			}
			if adopted, err := adoptPrepared(context.Background(), parent, "target", "stage", hook); err == nil {
				_ = adopted.Close()
				t.Fatal("parent swap adopted")
			}
			assertEmptyExactDirectory(t, filepath.Join(root, "held-parent", "stage"), identity)
			assertDirectoryEmpty(t, parent)
		})
	}
}

func TestAdoptPreparedCancellationRepeatedOpenAndDescriptorOwnership(t *testing.T) {
	previousGC := debug.SetGCPercent(-1)
	defer debug.SetGCPercent(previousGC)
	parent := secureTempDir(t)
	prepared := mustPrepare(t, parent, "target", "stage")
	identity := prepared.Identity()
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}

	cancelled, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := AdoptPrepared(cancelled, parent, "target", "stage"); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled adoption = %T %v", err, err)
	}
	assertEmptyExactDirectory(t, filepath.Join(parent, "stage"), identity)

	duringOpen, cancelDuringOpen := context.WithCancel(context.Background())
	hook := func(point materializePoint) error {
		if point.step == stepAfterAdoptOpen {
			cancelDuringOpen()
		}
		return nil
	}
	if _, err := adoptPrepared(duringOpen, parent, "target", "stage", hook); !errors.Is(err, context.Canceled) {
		t.Fatalf("adoption cancelled after open = %T %v", err, err)
	}
	assertEmptyExactDirectory(t, filepath.Join(parent, "stage"), identity)

	before := descriptorCount(t)
	for i := 0; i < 40; i++ {
		first, err := AdoptPrepared(context.Background(), parent, "target", "stage")
		if err != nil {
			t.Fatal(err)
		}
		second, err := AdoptPrepared(context.Background(), parent, "target", "stage")
		if err != nil {
			_ = first.Close()
			t.Fatal(err)
		}
		if !first.Identity().Equal(identity) || !second.Identity().Equal(identity) {
			t.Fatal("repeated adoption changed identity")
		}
		if err := first.Close(); err != nil {
			t.Fatal(err)
		}
		if err := second.Close(); err != nil {
			t.Fatal(err)
		}
	}
	if after := descriptorCount(t); after != before {
		t.Fatalf("descriptor count before=%d after=%d", before, after)
	}
	if created, err := Prepare(context.Background(), parent, "target", "stage"); err == nil {
		_ = created.Close()
		t.Fatal("create-only Prepare accepted retained stage")
	}
	assertEmptyExactDirectory(t, filepath.Join(parent, "stage"), identity)
}

func TestPrepareCrashReopenAndPostMkdirFailureRemainLocatable(t *testing.T) {
	parent := secureTempDir(t)
	format := mustFormat(t, "sha1")
	base := mustID(t, format, bytes.Repeat([]byte{3}, format.OIDLength()))
	empty := mustManifest(t, format, base, nil)

	prepared, err := Prepare(context.Background(), parent, "target", "crash-stage")
	if err != nil {
		t.Fatal(err)
	}
	identity := prepared.Identity()
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	facts, err := InspectPublished(context.Background(), parent, "crash-stage", identity, format, base)
	if err != nil {
		t.Fatal(err)
	}
	if !facts.Identity().Equal(identity) || facts.EntryCount() != 0 || facts.BlobBytes() != 0 || !facts.Commitment().Equal(empty.Commitment()) {
		t.Fatalf("reopened prepared facts differ: %+v", facts)
	}
	removeRecorded(t, parent, "crash-stage", identity)

	for i, step := range []materializeStep{stepAfterPrepareMkdir, stepBeforePrepareFsync} {
		name := fmt.Sprintf("failed-stage-%d", i)
		_, err = prepare(context.Background(), parent, "target", name, failAt(step))
		var unresolved *UnresolvedError
		if !errors.As(err, &unresolved) || !errors.Is(err, injectedFailure) {
			t.Fatalf("%s failure = %T %v, want retained UnresolvedError", step, err, err)
		}
		if unresolved.Parent != parent || unresolved.Name != name || !unresolved.HasIdentity || unresolved.Identity == (StageIdentity{}) {
			t.Fatalf("failure lost declared locator/identity: %+v", unresolved)
		}
		assertEmptyExactDirectory(t, filepath.Join(parent, name), unresolved.Identity)
		entries, readErr := os.ReadDir(parent)
		if readErr != nil || len(entries) != 1 || entries[0].Name() != name {
			t.Fatalf("Prepare created an undeclared/random artifact: %v %v", entries, readErr)
		}
		removeRecorded(t, parent, name, unresolved.Identity)
	}
}

func TestPrepareRejectsNonCanonicalLocatorsBeforeEffect(t *testing.T) {
	parent := secureTempDir(t)
	parents := []string{"relative", parent + "/", parent + "/child/..", parent + "//"}
	for _, candidate := range parents {
		t.Run(fmt.Sprintf("parent-%x", candidate), func(t *testing.T) {
			if _, err := Prepare(context.Background(), candidate, "target", "stage"); err == nil {
				t.Fatal("noncanonical parent accepted")
			}
			assertDirectoryEmpty(t, parent)
		})
	}
	for _, names := range [][2]string{
		{"", "stage"}, {".", "stage"}, {"..", "stage"}, {"a/b", "stage"}, {"a\\b", "stage"},
		{".GIT", "stage"}, {string([]byte{'b', 'a', 'd', 0xfe}), "stage"}, {"same", "same"}, {"target", "a/b"},
	} {
		t.Run(fmt.Sprintf("names-%x-%x", names[0], names[1]), func(t *testing.T) {
			if _, err := Prepare(context.Background(), parent, names[0], names[1]); err == nil {
				t.Fatal("unsafe target/stage accepted")
			}
			assertDirectoryEmpty(t, parent)
		})
	}
}

func TestPopulateAndInspectExactSHA1AndSHA256(t *testing.T) {
	for _, formatName := range []string{"sha1", "sha256"} {
		t.Run(formatName, func(t *testing.T) {
			parent := secureTempDir(t)
			fixture := newFixture(t, formatName, []fixtureFile{
				{[]byte("README.md"), "100644", []byte("hello\n")},
				{[]byte("bin/run"), "100755", []byte("#!/bin/sh\nexit 0\n")},
				{[]byte("empty"), "100644", nil},
				{[]byte("raw/e\u0301"), "100644", []byte("raw-name")},
			})
			prepared := mustPrepare(t, parent, "change", "stage")
			identity := prepared.Identity()
			published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
			if err != nil {
				t.Fatal(err)
			}
			if err := prepared.Close(); err != nil {
				t.Fatal(err)
			}
			facts := published.Facts()
			if !facts.Identity().Equal(identity) || !facts.Commitment().Equal(fixture.manifest.Commitment()) || facts.EntryCount() != fixture.manifest.EntryCount() || facts.BlobBytes() != fixture.manifest.BlobBytes() {
				t.Fatalf("publication facts differ: %+v", facts)
			}
			inspected, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
			if err != nil {
				t.Fatal(err)
			}
			if !inspected.Commitment().Equal(facts.Commitment()) || inspected.EntryCount() != facts.EntryCount() || inspected.BlobBytes() != facts.BlobBytes() {
				t.Fatalf("inspection facts differ: %+v vs %+v", inspected, facts)
			}
			assertExactTree(t, published.Path(), fixture)
		})
	}
}

func TestWideInspectionRefusesAtGlobalBudgetAndRemovalStaysBounded(t *testing.T) {
	if testing.Short() {
		t.Skip("creates a real over-limit recovery tree")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	parent := secureTempDir(t)
	prepared := mustPrepare(t, parent, "target", "wide-recorded")
	identity := prepared.Identity()
	root := filepath.Join(parent, "wide-recorded")
	const extra = 17
	for i := 0; i < int(MaxEntryCount)+extra; i++ {
		path := filepath.Join(root, fmt.Sprintf("entry-%05d", i))
		if err := os.WriteFile(path, nil, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	beforeFDs := descriptorCount(t)
	format := mustFormat(t, "sha1")
	base := mustID(t, format, bytes.Repeat([]byte{7}, format.OIDLength()))
	observed := 0
	_, err := inspectPublished(ctx, parent, "wide-recorded", identity, format, base, func(point materializePoint) error {
		if point.step == stepDuringDirectoryRead {
			observed++
		}
		return nil
	})
	var limit *LimitError
	if !errors.As(err, &limit) || observed != int(MaxEntryCount)+1 {
		t.Fatalf("wide inspection = %T %v observed=%d, want LimitError at %d", err, err, observed, MaxEntryCount+1)
	}
	if _, err := os.Stat(filepath.Join(root, fmt.Sprintf("entry-%05d", int(MaxEntryCount)+extra-1))); err != nil {
		t.Fatalf("bounded inspection mutated an unread suffix: %v", err)
	}

	census := &removalCensus{}
	if err := removeRecordedTreeWithCensus(ctx, parent, "wide-recorded", identity, nil, census); err != nil {
		t.Fatal(err)
	}
	if census.open != 0 || census.maximum > 3 {
		t.Fatalf("wide removal descriptor census open=%d maximum=%d, want 0 and <=3", census.open, census.maximum)
	}
	if afterFDs := descriptorCount(t); afterFDs != beforeFDs {
		t.Fatalf("oversized recovery leaked descriptors: before=%d after=%d", beforeFDs, afterFDs)
	}
	if _, err := os.Lstat(root); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("wide recorded tree survived removal: %v", err)
	}
}

func TestDeepRecoveryRemovalDoesNotRetainOneFDPerLevel(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	parent := secureTempDir(t)
	prepared := mustPrepare(t, parent, "target", "deep-recorded")
	identity := prepared.Identity()
	root := filepath.Join(parent, "deep-recorded")
	directory := root
	for depth := 0; depth < MaxDepth+32; depth++ {
		directory = filepath.Join(directory, "d")
		if err := os.Mkdir(directory, 0o700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(directory, "leaf"), []byte("provider-expanded"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	census := &removalCensus{}
	if err := removeRecordedTreeWithCensus(ctx, parent, "deep-recorded", identity, nil, census); err != nil {
		t.Fatal(err)
	}
	if census.open != 0 || census.maximum != 3 {
		t.Fatalf("deep removal descriptor census open=%d maximum=%d, want 0 and 3", census.open, census.maximum)
	}
	if _, err := os.Lstat(root); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("deep recorded tree survived removal: %v", err)
	}
}

func TestBeforeRenameCheckpointFollowsFsyncAndFinalRootVerification(t *testing.T) {
	t.Run("cancellation is the final pre-publication edge", func(t *testing.T) {
		parent := secureTempDir(t)
		fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("a")}})
		prepared := mustPrepare(t, parent, "change", "stage")
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		sawScan := false
		sawFsync := false
		renameChecks := 0
		hook := func(point materializePoint) error {
			switch point.step {
			case stepDuringTreeScan:
				sawScan = true
			case stepDuringTreeFsync:
				sawFsync = true
			case stepBeforeRename:
				renameChecks++
				if !sawScan || !sawFsync {
					t.Fatalf("rename edge preceded scan/fsync: scan=%v fsync=%v", sawScan, sawFsync)
				}
				cancel()
			}
			return nil
		}
		_, err := prepared.populateAndPublish(ctx, fixture.manifest, fixture.source, hook)
		var unknown *OutcomeUnknownError
		if !errors.Is(err, context.Canceled) || errors.As(err, &unknown) || renameChecks != 1 {
			t.Fatalf("rename-edge cancellation = %T %v checks=%d", err, err, renameChecks)
		}
		if _, err := os.Lstat(filepath.Join(parent, "change")); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("rename-edge cancellation published target: %v", err)
		}
		assertEmptyOrPopulatedDirectory(t, filepath.Join(parent, "stage"), prepared.Identity())
		removeRecorded(t, parent, "stage", prepared.Identity())
		_ = prepared.Close()
	})

	t.Run("root recheck separates fsync from rename edge", func(t *testing.T) {
		parent := secureTempDir(t)
		fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("a")}})
		prepared := mustPrepare(t, parent, "change", "stage")
		renameReached := false
		mutated := false
		hook := func(point materializePoint) error {
			if point.step == stepDuringTreeFsync && len(point.entryPath) == 0 && !mutated {
				mutated = true
				return unix.Chmod(filepath.Join(parent, "stage"), 0o777)
			}
			if point.step == stepBeforeRename {
				renameReached = true
			}
			return nil
		}
		if _, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, hook); err == nil || !mutated || renameReached {
			t.Fatalf("final root recheck missing: err=%v mutated=%v renameReached=%v", err, mutated, renameReached)
		}
		if _, err := os.Lstat(filepath.Join(parent, "change")); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("invalid root was published: %v", err)
		}
		_ = unix.Chmod(filepath.Join(parent, "stage"), 0o700)
		removeRecorded(t, parent, "stage", prepared.Identity())
		_ = prepared.Close()
	})
}

func TestInspectPublishedDetectsIdentityBaseBytesModesAndRootAuthority(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("right")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	identity := prepared.Identity()
	if _, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source); err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}

	wrongIdentity := StageIdentity{device: identity.device, inode: identity.inode + 1}
	if _, err := InspectPublished(context.Background(), parent, "change", wrongIdentity, fixture.manifest.format, fixture.manifest.base); err == nil {
		t.Fatal("wrong recorded identity accepted")
	}
	wrongBase := mustID(t, fixture.manifest.format, bytes.Repeat([]byte{8}, fixture.manifest.format.OIDLength()))
	wrongBaseFacts, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, wrongBase)
	if err != nil {
		t.Fatal(err)
	}
	if wrongBaseFacts.Commitment().Equal(fixture.manifest.Commitment()) {
		t.Fatal("wrong base did not change reconstructed commitment")
	}

	file := filepath.Join(parent, "change", "nested", "a")
	if err := os.WriteFile(file, []byte("wrong"), 0o644); err != nil {
		t.Fatal(err)
	}
	tampered, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
	if err != nil {
		t.Fatal(err)
	}
	if tampered.Commitment().Equal(fixture.manifest.Commitment()) {
		t.Fatal("same-size byte tamper trusted stored commitment")
	}
	// A provider under the service umask leaves 0600 files and 0700
	// directories, and one that runs chmod may leave 0755 of either; none of
	// those changes what is published. Group or other write and special bits
	// are refused, and the owner's execute bit is what the entry records.
	if err := unix.Chmod(file, 0o600); err != nil {
		t.Fatal(err)
	}
	private, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
	if err != nil || !private.Commitment().Equal(tampered.Commitment()) {
		t.Fatalf("private file mode: %v, commitment changed=%v", err, err == nil && !private.Commitment().Equal(tampered.Commitment()))
	}
	if err := unix.Chmod(filepath.Join(parent, "change", "nested"), 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base); err != nil {
		t.Fatalf("shared directory mode: %v", err)
	}
	if err := unix.Chmod(file, 0o700); err != nil {
		t.Fatal(err)
	}
	executable, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
	if err != nil || executable.Commitment().Equal(tampered.Commitment()) {
		t.Fatalf("owner-executable file: %v, commitment unchanged=%v", err, err == nil && executable.Commitment().Equal(tampered.Commitment()))
	}
	for _, refused := range []uint32{0o666, 0o664, 0o4755, 0o1644, 0o400} {
		if err := unix.Chmod(file, refused); err != nil {
			t.Fatal(err)
		}
		if _, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base); err == nil {
			t.Fatalf("file mode %o accepted", refused)
		}
	}
	if err := unix.Chmod(file, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := unix.Chmod(filepath.Join(parent, "change", "nested"), 0o775); err != nil {
		t.Fatal(err)
	}
	if _, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base); err == nil {
		t.Fatal("group-writable directory accepted")
	}
	if err := unix.Chmod(filepath.Join(parent, "change", "nested"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := unix.Chmod(filepath.Join(parent, "change"), 0o777); err != nil {
		t.Fatal(err)
	}
	if _, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base); err == nil {
		t.Fatal("root authority tamper accepted")
	}
}

// The inspection marks a refusal of the tree's own contents as Tree and a
// fault of the parent or the arguments as not: the daemon ends a run for
// the first and retries the second later.
func TestInspectPublishedMarksTreeRefusals(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("right")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	identity := prepared.Identity()
	if _, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source); err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	inspect := func() error {
		_, err := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
		return err
	}
	tree := func(err error) bool {
		var validation *ValidationError
		var limit *LimitError
		return errors.As(err, &validation) && validation.Tree || errors.As(err, &limit) && limit.Tree
	}
	root := filepath.Join(parent, "change")
	// An empty directory, a .git, a path too deep, a root the worker chmods:
	// the tree's own.
	for _, refusal := range []struct {
		name string
		make func()
		undo func()
	}{
		{"empty directory", func() { mustMkdir(t, filepath.Join(root, "left-empty")) }, func() { mustRemove(t, filepath.Join(root, "left-empty")) }},
		{".git", func() {
			mustMkdir(t, filepath.Join(root, ".git"))
			mustWrite(t, filepath.Join(root, ".git", "HEAD"), "ref")
		}, func() { mustRemoveAll(t, filepath.Join(root, ".git")) }},
		{"too deep", func() {
			deep := root
			for range MaxDepth + 1 {
				deep = filepath.Join(deep, "d")
			}
			if err := os.MkdirAll(deep, 0o700); err != nil {
				t.Fatal(err)
			}
			mustWrite(t, filepath.Join(deep, "leaf"), "x")
		}, func() { mustRemoveAll(t, filepath.Join(root, "d")) }},
		{"root mode", func() { mustChmod(t, root, 0o755) }, func() { mustChmod(t, root, 0o700) }},
	} {
		refusal.make()
		err := inspect()
		if err == nil || !tree(err) {
			t.Fatalf("%s: not a tree refusal: %v", refusal.name, err)
		}
		refusal.undo()
	}
	if err := inspect(); err != nil {
		t.Fatalf("tree restored: %v", err)
	}
	// The parent's mode is the environment's, not the tree's.
	mustChmod(t, parent, 0o755)
	if err := inspect(); err == nil || tree(err) {
		t.Fatalf("parent mode fault marked as the tree's: %v", err)
	}
	mustChmod(t, parent, 0o700)
	// A tree that is gone is not a refusal either.
	if err := os.Rename(root, root+".elsewhere"); err != nil {
		t.Fatal(err)
	}
	if err := inspect(); err == nil || tree(err) || !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("missing tree: %v", err)
	}
}

func mustMkdir(t *testing.T, path string) {
	t.Helper()
	if err := os.Mkdir(path, 0o700); err != nil {
		t.Fatal(err)
	}
}

func mustWrite(t *testing.T, path, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func mustRemove(t *testing.T, path string) {
	t.Helper()
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
}

func mustRemoveAll(t *testing.T, path string) {
	t.Helper()
	if err := os.RemoveAll(path); err != nil {
		t.Fatal(err)
	}
}

func mustChmod(t *testing.T, path string, mode os.FileMode) {
	t.Helper()
	if err := os.Chmod(path, mode); err != nil {
		t.Fatal(err)
	}
}

func TestPopulateRechecksRootAndNestedMetadataBeforeAndAfterRename(t *testing.T) {
	t.Run("root before blob", func(t *testing.T) {
		parent := secureTempDir(t)
		fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
		prepared := mustPrepare(t, parent, "change", "stage")
		if err := unix.Chmod(filepath.Join(parent, "stage"), 0o777); err != nil {
			t.Fatal(err)
		}
		called := false
		_, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, func(context.Context, ObjectID) ([]byte, error) {
			called = true
			return nil, nil
		})
		if err == nil || called {
			t.Fatalf("root mode was not rejected before blob read: err=%v called=%v", err, called)
		}
		_ = unix.Chmod(filepath.Join(parent, "stage"), 0o700)
		removeRecorded(t, parent, "stage", prepared.Identity())
		_ = prepared.Close()
	})

	t.Run("root special bits before blob", func(t *testing.T) {
		parent := secureTempDir(t)
		fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
		prepared := mustPrepare(t, parent, "change", "stage")
		if err := unix.Chmod(filepath.Join(parent, "stage"), 0o1700); err != nil {
			t.Fatal(err)
		}
		called := false
		_, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, func(context.Context, ObjectID) ([]byte, error) {
			called = true
			return nil, nil
		})
		if err == nil || called {
			t.Fatalf("root special bits were not rejected before blob read: err=%v called=%v", err, called)
		}
		_ = unix.Chmod(filepath.Join(parent, "stage"), 0o700)
		removeRecorded(t, parent, "stage", prepared.Identity())
		_ = prepared.Close()
	})

	for _, mutation := range []struct {
		name string
		path string
		mode uint32
	}{{"nested sticky", "nested", 0o1700}} {
		t.Run(mutation.name, func(t *testing.T) {
			parent := secureTempDir(t)
			fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("a")}})
			prepared := mustPrepare(t, parent, "change", "stage")
			hook := func(point materializePoint) error {
				if point.step == stepBeforeTreeFsync {
					return unix.Chmod(filepath.Join(point.parent, point.stagingName, mutation.path), mutation.mode)
				}
				return nil
			}
			if _, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, hook); err == nil {
				t.Fatal("special-bit mutation was published")
			}
			_ = unix.Chmod(filepath.Join(parent, "stage", mutation.path), mutation.mode&0o777)
			removeRecorded(t, parent, "stage", prepared.Identity())
			_ = prepared.Close()
		})
	}

	t.Run("root after rename", func(t *testing.T) {
		parent := secureTempDir(t)
		fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
		prepared := mustPrepare(t, parent, "change", "stage")
		hook := func(point materializePoint) error {
			if point.step == stepAfterRename {
				return unix.Chmod(filepath.Join(point.parent, point.targetName), 0o777)
			}
			return nil
		}
		_, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, hook)
		var unknown *OutcomeUnknownError
		if !errors.As(err, &unknown) {
			t.Fatalf("post-rename root mutation = %T %v, want OutcomeUnknownError", err, err)
		}
		_ = prepared.Close()
		if _, statErr := os.Stat(filepath.Join(parent, "change")); statErr != nil {
			t.Fatalf("ambiguous published target was deleted: %v", statErr)
		}
	})

	t.Run("nested special bits after rename", func(t *testing.T) {
		parent := secureTempDir(t)
		fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("a")}})
		prepared := mustPrepare(t, parent, "change", "stage")
		hook := func(point materializePoint) error {
			if point.step == stepAfterRename {
				return unix.Chmod(filepath.Join(point.parent, point.targetName, "nested"), 0o1700)
			}
			return nil
		}
		_, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, hook)
		var unknown *OutcomeUnknownError
		if !errors.As(err, &unknown) {
			t.Fatalf("post-rename nested mutation = %T %v, want OutcomeUnknownError", err, err)
		}
		_ = prepared.Close()
	})
}

func TestRootAuthorityPredicateRejectsOwnerModeSpecialBitsTypeAndIdentity(t *testing.T) {
	uid := uint32(os.Geteuid())
	identity := StageIdentity{device: 11, inode: 22}
	valid := unix.Stat_t{Dev: int32(identity.device), Ino: identity.inode, Uid: uid, Mode: unix.S_IFDIR | 0o700}
	if !rootAuthority(valid, identity, uid) {
		t.Fatal("valid root authority rejected")
	}
	mutations := map[string]func(*unix.Stat_t){
		"owner":        func(stat *unix.Stat_t) { stat.Uid++ },
		"mode":         func(stat *unix.Stat_t) { stat.Mode = unix.S_IFDIR | 0o777 },
		"special bits": func(stat *unix.Stat_t) { stat.Mode = unix.S_IFDIR | 0o1700 },
		"type":         func(stat *unix.Stat_t) { stat.Mode = unix.S_IFREG | 0o700 },
		"identity":     func(stat *unix.Stat_t) { stat.Ino++ },
	}
	for name, mutate := range mutations {
		t.Run(name, func(t *testing.T) {
			candidate := valid
			mutate(&candidate)
			if rootAuthority(candidate, identity, uid) {
				t.Fatalf("%s mutation accepted", name)
			}
		})
	}
}

func TestAdoptableStageAuthorityRequiresExactEmptyDirectoryLinkCount(t *testing.T) {
	uid := uint32(os.Geteuid())
	identity := StageIdentity{device: 11, inode: 22}
	valid := unix.Stat_t{Dev: int32(identity.device), Ino: identity.inode, Uid: uid, Mode: unix.S_IFDIR | 0o700, Nlink: 2}
	if !adoptableStageAuthority(valid, identity, uid) {
		t.Fatal("valid adoptable stage rejected")
	}
	for name, mutate := range map[string]func(*unix.Stat_t){
		"owner":      func(stat *unix.Stat_t) { stat.Uid++ },
		"mode":       func(stat *unix.Stat_t) { stat.Mode = unix.S_IFDIR | 0o755 },
		"type":       func(stat *unix.Stat_t) { stat.Mode = unix.S_IFREG | 0o700 },
		"identity":   func(stat *unix.Stat_t) { stat.Ino++ },
		"link count": func(stat *unix.Stat_t) { stat.Nlink++ },
	} {
		t.Run(name, func(t *testing.T) {
			candidate := valid
			mutate(&candidate)
			if adoptableStageAuthority(candidate, identity, uid) {
				t.Fatalf("%s mutation accepted", name)
			}
		})
	}
}

func TestCancellationInsideHashScanAndFsyncIsBounded(t *testing.T) {
	tests := []struct {
		name string
		step materializeStep
		data []byte
	}{{"blob hash", stepDuringBlobHash, bytes.Repeat([]byte("h"), 4<<20)}, {"tree scan", stepDuringTreeScan, bytes.Repeat([]byte("s"), 1<<20)}, {"directory fsync", stepDuringTreeFsync, []byte("f")}}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			parent := secureTempDir(t)
			fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", test.data}})
			prepared := mustPrepare(t, parent, "change", "stage")
			ctx, cancel := context.WithCancel(context.Background())
			entered := 0
			hook := func(point materializePoint) error {
				if point.step == test.step {
					entered++
					if entered == 1 {
						cancel()
					}
				}
				return nil
			}
			started := time.Now()
			_, err := prepared.populateAndPublish(ctx, fixture.manifest, fixture.source, hook)
			if !errors.Is(err, context.Canceled) || entered != 1 || time.Since(started) > 2*time.Second {
				t.Fatalf("loop cancellation failed: err=%v entered=%d duration=%s", err, entered, time.Since(started))
			}
			if _, statErr := os.Lstat(filepath.Join(parent, "change")); !errors.Is(statErr, os.ErrNotExist) {
				t.Fatalf("canceled population published: %v", statErr)
			}
			removeRecorded(t, parent, "stage", prepared.Identity())
			_ = prepared.Close()
		})
	}
}

func TestPopulateRejectsWrongBlobBeforeFilesystemCreation(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("right")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	createReached := false
	_, err := prepared.populateAndPublish(context.Background(), fixture.manifest, func(context.Context, ObjectID) ([]byte, error) {
		return []byte("wrong"), nil
	}, func(point materializePoint) error {
		if point.step == stepBeforeEntryParentOpen || point.step == stepBeforeFileCreate {
			createReached = true
		}
		return nil
	})
	if err == nil || createReached {
		t.Fatalf("wrong blob reached filesystem creation: err=%v reached=%v", err, createReached)
	}
	assertEmptyExactDirectory(t, filepath.Join(parent, "stage"), prepared.Identity())
	removeRecorded(t, parent, "stage", prepared.Identity())
	_ = prepared.Close()
}

func TestPopulateRejectsNonPlainEntriesAndSymlinkEscape(t *testing.T) {
	injections := map[string]func(string) error{
		"symlink":         func(stage string) error { return os.Symlink("/private/tmp", filepath.Join(stage, "extra")) },
		"hardlink":        func(stage string) error { return os.Link(filepath.Join(stage, "a"), filepath.Join(stage, "extra")) },
		"fifo":            func(stage string) error { return unix.Mkfifo(filepath.Join(stage, "extra"), 0o600) },
		"empty directory": func(stage string) error { return os.Mkdir(filepath.Join(stage, "extra"), 0o700) },
		"socket": func(stage string) error {
			listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: filepath.Join(stage, "extra"), Net: "unix"})
			if err != nil {
				return err
			}
			listener.SetUnlinkOnClose(false)
			return listener.Close()
		},
	}
	for name, inject := range injections {
		t.Run(name, func(t *testing.T) {
			parent := secureTempDir(t)
			fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
			prepared := mustPrepare(t, parent, "change", "stage")
			hook := func(point materializePoint) error {
				if point.step == stepBeforeTreeVerify {
					return inject(filepath.Join(point.parent, point.stagingName))
				}
				return nil
			}
			if _, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, hook); err == nil {
				t.Fatalf("injected %s was accepted", name)
			}
			removeRecorded(t, parent, "stage", prepared.Identity())
			_ = prepared.Close()
		})
	}

	parent := secureTempDir(t)
	outside := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a-seed"), "100644", []byte("seed")}, {[]byte("nested/z-payload"), "100644", []byte("secret")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	swapped := false
	hook := func(point materializePoint) error {
		if point.step != stepBeforeEntryParentOpen || string(point.entryPath) != "nested/z-payload" || swapped {
			return nil
		}
		swapped = true
		stage := filepath.Join(point.parent, point.stagingName)
		if err := os.Rename(filepath.Join(stage, "nested"), filepath.Join(stage, "owned-moved")); err != nil {
			return err
		}
		return os.Symlink(outside, filepath.Join(stage, "nested"))
	}
	if _, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, hook); err == nil {
		t.Fatal("intermediate symlink swap was accepted")
	}
	if _, err := os.Lstat(filepath.Join(outside, "z-payload")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("write escaped staging root: %v", err)
	}
	removeRecorded(t, parent, "stage", prepared.Identity())
	_ = prepared.Close()
}

func TestCreateOnlyTargetsAndConcurrentPublish(t *testing.T) {
	for _, kind := range []string{"file", "directory", "symlink"} {
		t.Run(kind, func(t *testing.T) {
			parent := secureTempDir(t)
			target := filepath.Join(parent, "change")
			switch kind {
			case "file":
				if err := os.WriteFile(target, []byte("sentinel"), 0o600); err != nil {
					t.Fatal(err)
				}
			case "directory":
				if err := os.Mkdir(target, 0o700); err != nil {
					t.Fatal(err)
				}
			case "symlink":
				if err := os.Symlink("sentinel", target); err != nil {
					t.Fatal(err)
				}
			}
			before, _ := os.Lstat(target)
			if _, err := Prepare(context.Background(), parent, "change", "stage"); err == nil {
				t.Fatal("existing target accepted")
			}
			after, err := os.Lstat(target)
			if err != nil || !os.SameFile(before, after) {
				t.Fatalf("existing target changed: %v", err)
			}
			if _, err := os.Lstat(filepath.Join(parent, "stage")); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("conflict created staging: %v", err)
			}
		})
	}

	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("winner"), "100644", []byte("exact")}})
	first := mustPrepare(t, parent, "change", "stage-one")
	second := mustPrepare(t, parent, "change", "stage-two")
	start := make(chan struct{})
	results := make(chan error, 2)
	var wait sync.WaitGroup
	for _, prepared := range []*Prepared{first, second} {
		wait.Add(1)
		go func(prepared *Prepared) {
			defer wait.Done()
			<-start
			_, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
			results <- err
		}(prepared)
	}
	close(start)
	wait.Wait()
	close(results)
	var success, conflict int
	for err := range results {
		if err == nil {
			success++
			continue
		}
		var conflictError *ConflictError
		if errors.As(err, &conflictError) {
			conflict++
			continue
		}
		t.Fatalf("unexpected concurrent error: %T %v", err, err)
	}
	if success != 1 || conflict != 1 {
		t.Fatalf("success=%d conflict=%d, want one each", success, conflict)
	}
	assertExactTree(t, filepath.Join(parent, "change"), fixture)
	removeRecorded(t, parent, "stage-one", first.Identity())
	removeRecorded(t, parent, "stage-two", second.Identity())
	_ = first.Close()
	_ = second.Close()
}

func TestPostPublicationFailureIsInspectedNeverDeleted(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("right")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	identity := prepared.Identity()
	_, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, failAt(stepAfterRename))
	var unknown *OutcomeUnknownError
	if !errors.As(err, &unknown) || !errors.Is(err, injectedFailure) || !unknown.Identity.Equal(identity) || !unknown.Commitment.Equal(fixture.manifest.Commitment()) {
		t.Fatalf("post-publication cut = %T %v, want exact OutcomeUnknownError", err, err)
	}
	facts, inspectErr := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
	if inspectErr != nil || !facts.Commitment().Equal(fixture.manifest.Commitment()) {
		t.Fatalf("reconciliation failed: facts=%+v err=%v", facts, inspectErr)
	}
	_ = prepared.Close()

	parent = secureTempDir(t)
	prepared = mustPrepare(t, parent, "change", "stage")
	identity = prepared.Identity()
	hook := func(point materializePoint) error {
		if point.step == stepAfterRename {
			return os.WriteFile(filepath.Join(point.parent, point.targetName, "a"), []byte("wrong"), 0o644)
		}
		return nil
	}
	_, err = prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, hook)
	if !errors.As(err, &unknown) {
		t.Fatalf("post-publication tamper = %T %v, want OutcomeUnknownError", err, err)
	}
	tampered, inspectErr := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
	if inspectErr != nil || tampered.Commitment().Equal(fixture.manifest.Commitment()) {
		t.Fatalf("inspection trusted expected digest: facts=%+v err=%v", tampered, inspectErr)
	}
	_ = prepared.Close()
}

func TestPopulateFailureCutsLeaveOnlyTheDeclaredRecordedStage(t *testing.T) {
	prePublication := []materializeStep{
		stepBeforeFileCreate, stepBeforeFileWrite, stepBeforeFileFsync,
		stepBeforeTreeVerify, stepBeforeTreeFsync, stepBeforeRename,
	}
	for _, step := range prePublication {
		t.Run(string(step), func(t *testing.T) {
			parent := secureTempDir(t)
			fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
			prepared := mustPrepare(t, parent, "change", "declared-stage")
			_, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, failAt(step))
			if !errors.Is(err, injectedFailure) {
				t.Fatalf("cut = %T %v, want injected failure", err, err)
			}
			if _, err := os.Lstat(filepath.Join(parent, "change")); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("pre-publication cut created target: %v", err)
			}
			entries, readErr := os.ReadDir(parent)
			if readErr != nil || len(entries) != 1 || entries[0].Name() != "declared-stage" {
				t.Fatalf("cut path census=%v err=%v", entries, readErr)
			}
			removeRecorded(t, parent, "declared-stage", prepared.Identity())
			_ = prepared.Close()
		})
	}
	for _, step := range []materializeStep{stepAfterRename, stepBeforeParentFsync} {
		t.Run(string(step), func(t *testing.T) {
			parent := secureTempDir(t)
			fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
			prepared := mustPrepare(t, parent, "change", "declared-stage")
			identity := prepared.Identity()
			_, err := prepared.populateAndPublish(context.Background(), fixture.manifest, fixture.source, failAt(step))
			var unknown *OutcomeUnknownError
			if !errors.As(err, &unknown) || !errors.Is(err, injectedFailure) || !unknown.Identity.Equal(identity) {
				t.Fatalf("post-publication cut = %T %v, want OutcomeUnknownError", err, err)
			}
			facts, inspectErr := InspectPublished(context.Background(), parent, "change", identity, fixture.manifest.format, fixture.manifest.base)
			if inspectErr != nil || !facts.Commitment().Equal(fixture.manifest.Commitment()) {
				t.Fatalf("post-cut reconciliation facts=%+v err=%v", facts, inspectErr)
			}
			_ = prepared.Close()
		})
	}
}

func TestRemoveRecordedTreeExactMismatchAndSwap(t *testing.T) {
	parent := secureTempDir(t)
	prepared := mustPrepare(t, parent, "target", "stage")
	identity := prepared.Identity()
	if err := os.WriteFile(filepath.Join(parent, "stage", "sentinel"), []byte("keep"), 0o644); err != nil {
		t.Fatal(err)
	}
	wrong := StageIdentity{device: identity.device, inode: identity.inode + 1}
	if err := RemoveRecordedTree(context.Background(), parent, "stage", wrong); err == nil {
		t.Fatal("identity mismatch authorized removal")
	}
	if _, err := os.Stat(filepath.Join(parent, "stage", "sentinel")); err != nil {
		t.Fatalf("mismatch changed artifact: %v", err)
	}
	if err := RemoveRecordedTree(context.Background(), parent, "stage", identity); err != nil {
		t.Fatal(err)
	}
	if err := RemoveRecordedTree(context.Background(), parent, "stage", identity); err != nil {
		t.Fatalf("positive absence was not idempotent: %v", err)
	}
	_ = prepared.Close()

	prepared = mustPrepare(t, parent, "target", "swap-stage")
	identity = prepared.Identity()
	preserved := filepath.Join(parent, "preserved")
	hook := func(point materializePoint) error {
		if point.step == stepBeforeRecordedRemoval {
			if err := os.Rename(filepath.Join(parent, "swap-stage"), preserved); err != nil {
				return err
			}
			return os.Mkdir(filepath.Join(parent, "swap-stage"), 0o700)
		}
		return nil
	}
	err := removeRecordedTree(context.Background(), parent, "swap-stage", identity, hook)
	var unresolved *UnresolvedError
	if !errors.As(err, &unresolved) {
		t.Fatalf("identity swap = %T %v, want UnresolvedError", err, err)
	}
	if _, err := os.Stat(preserved); err != nil {
		t.Fatalf("original artifact removed after swap: %v", err)
	}
	if _, err := os.Stat(filepath.Join(parent, "swap-stage")); err != nil {
		t.Fatalf("replacement artifact removed after swap: %v", err)
	}
	_ = prepared.Close()
}

func TestPublishedChangeCarriesNoRepositoryAuthority(t *testing.T) {
	parent := secureTempDir(t)
	git(t, parent, "init", "--quiet")
	changes := filepath.Join(parent, "changes")
	if err := os.Mkdir(changes, 0o700); err != nil {
		t.Fatal(err)
	}
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
	prepared := mustPrepare(t, changes, "change", "stage")
	published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
	if err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Lstat(filepath.Join(published.Path(), ".git")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("published Change has Git metadata: %v", err)
	}
	command := exec.Command("git", "rev-parse", "--is-inside-work-tree")
	command.Dir = published.Path()
	command.Env = append(safeGitEnv(), "GIT_CEILING_DIRECTORIES="+changes)
	if output, err := command.CombinedOutput(); err == nil {
		t.Fatalf("controlled ceiling discovered ancestor repository: %q", output)
	}
	command = exec.Command("git", "rev-parse", "--is-inside-work-tree")
	command.Dir = published.Path()
	command.Env = safeGitEnv()
	output, err := command.Output()
	if err != nil || strings.TrimSpace(string(output)) != "true" {
		t.Fatalf("fixture did not prove ancestor discovery without ceiling: %q %v", output, err)
	}
}

func TestPublishedReinspectRetainsExactDirectoryOwnership(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("right")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
	if err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}

	baseline := descriptorCount(t)
	verified, err := published.Reinspect(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if formatted := fmt.Sprintf("%v %+v %#v %v %+v %#v", published, published, published, verified, verified, verified); formatted != "Published(<redacted>) Published(<redacted>) Published(<redacted>) VerifiedPublished(<redacted>) VerifiedPublished(<redacted>) VerifiedPublished(<redacted>)" {
		t.Fatalf("capability formatting exposed authority: %q", formatted)
	}
	facts, err := verified.Facts()
	if err != nil || facts != published.Facts() {
		t.Fatalf("verified facts = %+v, %v; want %+v", facts, err, published.Facts())
	}

	first, err := verified.DuplicateDirectory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	var firstStat unix.Stat_t
	if err := unix.Fstat(int(first.Fd()), &firstStat); err != nil || identityOf(firstStat) != published.Facts().Identity() {
		t.Fatalf("first duplicate identity = %+v, %v", identityOf(firstStat), err)
	}

	moved := filepath.Join(parent, "retained-original")
	if err := os.Rename(published.Path(), moved); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(published.Path(), 0o700); err != nil {
		t.Fatal(err)
	}
	second, err := verified.DuplicateDirectory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	var secondStat, replacementStat unix.Stat_t
	if err := unix.Fstat(int(second.Fd()), &secondStat); err != nil {
		t.Fatal(err)
	}
	if err := unix.Lstat(published.Path(), &replacementStat); err != nil {
		t.Fatal(err)
	}
	if identityOf(secondStat) != published.Facts().Identity() || identityOf(secondStat) == identityOf(replacementStat) {
		t.Fatalf("duplicate retargeted: duplicate=%+v original=%+v replacement=%+v", identityOf(secondStat), published.Facts().Identity(), identityOf(replacementStat))
	}

	copyOfCapability := *verified
	if err := verified.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := copyOfCapability.Facts(); !isLifecycle(err) {
		t.Fatalf("copied capability survived shared close: %T %v", err, err)
	}
	if _, err := copyOfCapability.DuplicateDirectory(context.Background()); !isLifecycle(err) {
		t.Fatalf("duplicate after close = %T %v, want LifecycleError", err, err)
	}
	if err := copyOfCapability.Close(); !isLifecycle(err) {
		t.Fatalf("second close = %T %v, want LifecycleError", err, err)
	}
	for _, directory := range []*os.File{first, second} {
		var stat unix.Stat_t
		if err := unix.Fstat(int(directory.Fd()), &stat); err != nil || identityOf(stat) != published.Facts().Identity() {
			t.Fatalf("owned duplicate did not survive capability close: %+v %v", identityOf(stat), err)
		}
		if err := directory.Close(); err != nil {
			t.Fatal(err)
		}
	}
	if after := descriptorCount(t); after != baseline {
		t.Fatalf("verified ownership leaked descriptors: before=%d after=%d", baseline, after)
	}
}

func TestPublishedReinspectRejectsLateTreeMutation(t *testing.T) {
	t.Run("same-size content", func(t *testing.T) {
		parent := secureTempDir(t)
		fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("nested/a"), "100644", []byte("right")}})
		prepared := mustPrepare(t, parent, "change", "stage")
		published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
		if err != nil {
			t.Fatal(err)
		}
		if err := prepared.Close(); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(published.Path(), "nested", "a"), []byte("wrong"), 0o644); err != nil {
			t.Fatal(err)
		}
		if verified, err := published.Reinspect(context.Background()); err == nil || verified != nil {
			t.Fatalf("same-size mutation retained authority: %+v, %v", verified, err)
		}
	})

	for _, name := range []string{".git", ".GiT"} {
		t.Run(fmt.Sprintf("forbidden-%x", name), func(t *testing.T) {
			parent := secureTempDir(t)
			fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
			prepared := mustPrepare(t, parent, "change", "stage")
			published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
			if err != nil {
				t.Fatal(err)
			}
			if err := prepared.Close(); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(filepath.Join(published.Path(), "nested"), 0o700); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(filepath.Join(published.Path(), "nested", name), 0o700); err != nil {
				t.Fatal(err)
			}
			if verified, err := published.Reinspect(context.Background()); err == nil || verified != nil {
				t.Fatalf("late %q retained authority: %+v, %v", name, verified, err)
			}
		})
	}
}

func TestPublishedReinspectRejectsCorruptCapabilityFactsAndDescriptor(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
	if err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}

	mutations := map[string]func(*Published){
		"entry count": func(value *Published) { value.facts.entryCount++ },
		"blob bytes":  func(value *Published) { value.facts.blobBytes++ },
		"commitment":  func(value *Published) { value.facts.commitment = Commitment{} },
		"identity":    func(value *Published) { value.facts.identity.inode++ },
		"base": func(value *Published) {
			value.base = mustID(t, value.format, bytes.Repeat([]byte{0x77}, value.format.OIDLength()))
		},
		"format": func(value *Published) { value.format = ObjectFormat(0) },
		"path":   func(value *Published) { value.path = filepath.Join(parent, "alias") },
	}
	for name, mutate := range mutations {
		t.Run(name, func(t *testing.T) {
			corrupt := published
			mutate(&corrupt)
			if verified, err := corrupt.Reinspect(context.Background()); err == nil || verified != nil {
				t.Fatalf("corrupt capability retained authority: %+v, %v", verified, err)
			}
		})
	}
	if verified, err := (Published{}).Reinspect(context.Background()); err == nil || verified != nil {
		t.Fatalf("zero Published retained authority: %+v, %v", verified, err)
	}
	zero := &VerifiedPublished{}
	if _, err := zero.Facts(); !isLifecycle(err) {
		t.Fatalf("zero verified facts = %T %v", err, err)
	}
	if _, err := zero.DuplicateDirectory(context.Background()); !isLifecycle(err) {
		t.Fatalf("zero verified duplicate = %T %v", err, err)
	}
	if err := zero.Close(); !isLifecycle(err) {
		t.Fatalf("zero verified close = %T %v", err, err)
	}

	verified, err := published.Reinspect(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if err := unix.Chmod(published.Path(), 0o777); err != nil {
		t.Fatal(err)
	}
	if facts, err := verified.Facts(); err == nil || facts != (TreeFacts{}) {
		t.Fatalf("invalid root metadata returned facts: %+v, %v", facts, err)
	}
	if duplicate, err := verified.DuplicateDirectory(context.Background()); err == nil || duplicate != nil {
		t.Fatalf("invalid root metadata authorized duplicate: %+v, %v", duplicate, err)
	}
	if err := unix.Chmod(published.Path(), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := verified.state.directory.Close(); err != nil {
		t.Fatal(err)
	}
	if duplicate, err := verified.DuplicateDirectory(context.Background()); err == nil || duplicate != nil || strings.Contains(fmt.Sprint(err), parent) {
		t.Fatalf("closed retained descriptor authorized duplicate or leaked path: %+v, %v", duplicate, err)
	}
	_ = verified.Close()
}

func TestPublishedInspectionReplacementAndCancellationCloseDescriptors(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
	prepared := mustPrepare(t, parent, "change", "stage")
	published, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source)
	if err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	baseline := descriptorCount(t)
	baselineGoroutines := runtime.NumGoroutine()

	ctx, cancel := context.WithCancel(context.Background())
	visited := false
	facts := published.Facts()
	_, directory, err := inspectPublishedDirectory(ctx, published.parent, published.target, facts.identity, published.format, published.base, func(point materializePoint) error {
		if point.step == stepDuringTreeScan {
			visited = true
			cancel()
		}
		return nil
	})
	if !visited || !errors.Is(err, context.Canceled) || directory != nil {
		t.Fatalf("scan cancellation = visited=%v directory=%+v err=%v", visited, directory, err)
	}
	if after := descriptorCount(t); after != baseline {
		t.Fatalf("canceled scan leaked descriptors: before=%d after=%d", baseline, after)
	}

	moved := filepath.Join(parent, "moved-during-scan")
	replaced := false
	_, directory, err = inspectPublishedDirectory(context.Background(), published.parent, published.target, facts.identity, published.format, published.base, func(point materializePoint) error {
		if point.step == stepDuringTreeScan && !replaced {
			replaced = true
			if err := os.Rename(published.Path(), moved); err != nil {
				return err
			}
			return os.Mkdir(published.Path(), 0o700)
		}
		return nil
	})
	if !replaced || err == nil || directory != nil {
		t.Fatalf("replacement during scan retained authority: replaced=%v directory=%+v err=%v", replaced, directory, err)
	}
	if after := descriptorCount(t); after != baseline {
		t.Fatalf("replacement scan leaked descriptors: before=%d after=%d", baseline, after)
	}

	if err := os.RemoveAll(published.Path()); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(moved, published.Path()); err != nil {
		t.Fatal(err)
	}
	verified, err := published.Reinspect(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	canceled, stop := context.WithCancel(context.Background())
	stop()
	if duplicate, err := verified.DuplicateDirectory(canceled); !errors.Is(err, context.Canceled) || duplicate != nil {
		t.Fatalf("canceled duplicate = %+v, %v", duplicate, err)
	}
	if err := verified.Close(); err != nil {
		t.Fatal(err)
	}
	if after := descriptorCount(t); after != baseline {
		t.Fatalf("verified cancellation leaked descriptors: before=%d after=%d", baseline, after)
	}
	if after := runtime.NumGoroutine(); after > baselineGoroutines {
		t.Fatalf("verified lifecycle leaked goroutines: before=%d after=%d", baselineGoroutines, after)
	}
}

func TestPreparedLifecycleDoesNotLeakResources(t *testing.T) {
	parent := secureTempDir(t)
	fixture := newFixture(t, "sha1", []fixtureFile{{[]byte("a"), "100644", []byte("a")}})
	beforeFDs := descriptorCount(t)
	beforeGoroutines := runtime.NumGoroutine()
	for i := range 20 {
		prepared := mustPrepare(t, parent, fmt.Sprintf("change-%d", i), fmt.Sprintf("stage-%d", i))
		if _, err := prepared.PopulateAndPublish(context.Background(), fixture.manifest, fixture.source); err != nil {
			t.Fatal(err)
		}
		if err := prepared.Close(); err != nil {
			t.Fatal(err)
		}
	}
	runtime.GC()
	if after := descriptorCount(t); after != beforeFDs {
		t.Fatalf("descriptor count changed: before=%d after=%d", beforeFDs, after)
	}
	if after := runtime.NumGoroutine(); after > beforeGoroutines+1 {
		t.Fatalf("goroutine count changed: before=%d after=%d", beforeGoroutines, after)
	}
	entries, err := os.ReadDir(parent)
	if err != nil || len(entries) != 20 {
		t.Fatalf("unexpected path census: count=%d err=%v", len(entries), err)
	}
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), "stage-") {
			t.Fatalf("staging locator survived publication: %q", entry.Name())
		}
	}
}

type fixtureFile struct {
	path []byte
	mode string
	data []byte
}

func newFixture(t testing.TB, formatName string, files []fixtureFile) changeFixture {
	t.Helper()
	format := mustFormat(t, formatName)
	base := mustID(t, format, bytes.Repeat([]byte{0x55}, format.OIDLength()))
	entries := make([]Entry, len(files))
	blobs := make(map[string][]byte, len(files))
	for i, file := range files {
		entries[i] = mustEntry(t, format, file.path, file.mode, file.data)
		blobs[entries[i].oid.Hex()] = bytes.Clone(file.data)
	}
	return changeFixture{manifest: mustManifest(t, format, base, entries), blobs: blobs}
}

func (f changeFixture) source(_ context.Context, oid ObjectID) ([]byte, error) {
	data, ok := f.blobs[oid.Hex()]
	if !ok {
		return nil, errors.New("missing fixture blob")
	}
	return bytes.Clone(data), nil
}

func mustPrepare(t testing.TB, parent, target, staging string) *Prepared {
	t.Helper()
	prepared, err := Prepare(context.Background(), parent, target, staging)
	if err != nil {
		t.Fatal(err)
	}
	return prepared
}

func removeRecorded(t testing.TB, parent, name string, identity StageIdentity) {
	t.Helper()
	if err := RemoveRecordedTree(context.Background(), parent, name, identity); err != nil {
		t.Fatal(err)
	}
}

func secureTempDir(t testing.TB) string {
	t.Helper()
	path, err := os.MkdirTemp("/private/tmp", "dark-factory-change-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(path) })
	return path
}

func failAt(step materializeStep) materializeHook {
	return func(point materializePoint) error {
		if point.step == step {
			return injectedFailure
		}
		return nil
	}
}

func isLifecycle(err error) bool { var lifecycle *LifecycleError; return errors.As(err, &lifecycle) }

func assertEmptyExactDirectory(t testing.TB, path string, identity StageIdentity) {
	t.Helper()
	assertExactDirectoryIdentity(t, path, identity)
	entries, err := os.ReadDir(path)
	if err != nil || len(entries) != 0 {
		t.Fatalf("prepared stage not empty: %v %v", entries, err)
	}
}

func assertEmptyOrPopulatedDirectory(t testing.TB, path string, identity StageIdentity) {
	t.Helper()
	assertExactDirectoryIdentity(t, path, identity)
}

func assertExactDirectoryIdentity(t testing.TB, path string, identity StageIdentity) {
	t.Helper()
	info, err := os.Lstat(path)
	if err != nil {
		t.Fatal(err)
	}
	if !info.IsDir() || info.Mode().Perm() != 0o700 {
		t.Fatalf("prepared stage mode=%v", info.Mode())
	}
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer unix.Close(fd)
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		t.Fatal(err)
	}
	if !identityOf(stat).Equal(identity) {
		t.Fatalf("stage identity differs: %+v", identityOf(stat))
	}
}

func assertDirectoryEmpty(t testing.TB, path string) {
	t.Helper()
	entries, err := os.ReadDir(path)
	if err != nil || len(entries) != 0 {
		t.Fatalf("directory has effects: %v %v", entries, err)
	}
}

func assertExactTree(t testing.TB, root string, fixture changeFixture) {
	t.Helper()
	actualDirs := make([]string, 0)
	actualFiles := make(map[string]struct{})
	err := filepath.WalkDir(root, func(path string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if path == root {
			return nil
		}
		relative, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		info, err := os.Lstat(path)
		if err != nil {
			return err
		}
		if info.Mode()&os.ModeSymlink != 0 || info.Mode()&(os.ModeDevice|os.ModeNamedPipe|os.ModeSocket) != 0 {
			return fmt.Errorf("non-plain entry %q", relative)
		}
		if entry.IsDir() {
			if info.Mode().Perm() != 0o700 {
				return fmt.Errorf("directory mode %q=%o", relative, info.Mode().Perm())
			}
			actualDirs = append(actualDirs, relative)
			return nil
		}
		if !info.Mode().IsRegular() {
			return fmt.Errorf("non-regular file %q", relative)
		}
		actualFiles[relative] = struct{}{}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	slices.Sort(actualDirs)
	if !slices.Equal(actualDirs, fixture.manifest.directories) {
		t.Fatalf("directories=%q want=%q", actualDirs, fixture.manifest.directories)
	}
	if len(actualFiles) != len(fixture.manifest.entries) {
		t.Fatalf("file count=%d", len(actualFiles))
	}
	for _, entry := range fixture.manifest.entries {
		path := filepath.Join(root, string(entry.path))
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(data, fixture.blobs[entry.oid.Hex()]) {
			t.Fatalf("bytes differ for %x", entry.path)
		}
		info, err := os.Lstat(path)
		if err != nil {
			t.Fatal(err)
		}
		want := os.FileMode(0o644)
		if entry.mode == "100755" {
			want = 0o755
		}
		if info.Mode().Perm() != want {
			t.Fatalf("mode for %x=%o want=%o", entry.path, info.Mode().Perm(), want)
		}
	}
}

func git(t testing.TB, directory string, args ...string) {
	t.Helper()
	command := exec.Command("git", args...)
	command.Dir = directory
	command.Env = safeGitEnv()
	if output, err := command.CombinedOutput(); err != nil {
		t.Fatalf("git %v: %v: %s", args, err, output)
	}
}

func safeGitEnv() []string {
	return []string{"PATH=/usr/bin:/bin", "HOME=/nonexistent", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null", "GIT_TERMINAL_PROMPT=0"}
}

func descriptorCount(t testing.TB) int {
	t.Helper()
	entries, err := os.ReadDir("/dev/fd")
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}
