package kernel

import (
	"bytes"
	"context"
	"database/sql"
	"database/sql/driver"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	sqliteDriver "github.com/ncruces/go-sqlite3/driver"
)

const (
	kernelCrashModeEnv     = "DARK_FACTORY_KERNEL_CRASH_MODE"
	kernelCrashPathEnv     = "DARK_FACTORY_KERNEL_CRASH_PATH"
	kernelCrashRunEnv      = "DARK_FACTORY_KERNEL_CRASH_RUN"
	kernelCrashRevisionEnv = "DARK_FACTORY_KERNEL_CRASH_REVISION"
)

type crashCommitConnector struct {
	driver.Connector
	after bool
}

func (connector *crashCommitConnector) Connect(ctx context.Context) (driver.Conn, error) {
	connection, err := connector.Connector.Connect(ctx)
	if err != nil {
		return nil, err
	}
	return &crashCommitConnection{Conn: connection, after: connector.after}, nil
}

type crashCommitConnection struct {
	driver.Conn
	after bool
}

func (connection *crashCommitConnection) ExecContext(ctx context.Context, query string, args []driver.NamedValue) (driver.Result, error) {
	execer, ok := connection.Conn.(driver.ExecerContext)
	if !ok {
		return nil, driver.ErrSkip
	}
	if query != "COMMIT" {
		return execer.ExecContext(ctx, query, args)
	}
	if !connection.after {
		blockCrashHelper()
	}
	result, err := execer.ExecContext(ctx, query, args)
	if err != nil {
		return result, err
	}
	blockCrashHelper()
	return result, nil
}

func blockCrashHelper() {
	ready := os.NewFile(3, "kernel-crash-ready")
	hold := os.NewFile(4, "kernel-crash-hold")
	if ready == nil || hold == nil {
		fmt.Fprintln(os.Stderr, "missing crash helper pipes")
		os.Exit(2)
	}
	if _, err := ready.Write([]byte{1}); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	_ = ready.Close()
	var byte [1]byte
	if _, err := io.ReadFull(hold, byte[:]); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	os.Exit(3)
}

func installCrashWriter(store *Store, path string, after bool) error {
	base, err := (&sqliteDriver.SQLite{}).OpenConnector(configuredDataSource(path))
	if err != nil {
		return err
	}
	pool := sql.OpenDB(&crashCommitConnector{Connector: base, after: after})
	pool.SetMaxOpenConns(1)
	pool.SetMaxIdleConns(1)
	if err := pool.PingContext(context.Background()); err != nil {
		pool.Close()
		return err
	}
	old := store.writer
	store.writer = pool
	return old.Close()
}

func TestKernelConcreteCrashHelper(t *testing.T) {
	mode := os.Getenv(kernelCrashModeEnv)
	if mode == "" {
		return
	}
	path := os.Getenv(kernelCrashPathEnv)
	store, err := Open(context.Background(), path)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	if err := installCrashWriter(store, path, strings.HasSuffix(mode, "-after")); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	switch {
	case strings.HasPrefix(mode, "admit-"):
		_, err = store.AdmitNext(context.Background(), admissionKeys(t, 230, nil), mustTime(t, 10))
	case strings.HasPrefix(mode, "finalize-"):
		raw, decodeErr := hex.DecodeString(os.Getenv(kernelCrashRunEnv))
		if decodeErr != nil {
			err = decodeErr
			break
		}
		runID, decodeErr := RunIDFromBytes(raw)
		if decodeErr != nil {
			err = decodeErr
			break
		}
		revision, parseErr := strconv.ParseInt(os.Getenv(kernelCrashRevisionEnv), 10, 64)
		if parseErr != nil {
			err = parseErr
			break
		}
		_, err = store.FinalizeRun(context.Background(), runID, mustRevision(t, revision), mustTime(t, 100))
	default:
		err = fmt.Errorf("unknown crash mode %q", mode)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	os.Exit(3)
}

type kernelCrashHelper struct {
	command *exec.Cmd
	ready   *os.File
	hold    *os.File
	stderr  bytes.Buffer
}

func startKernelCrashHelper(t *testing.T, mode, path string, runID RunID, revision Revision) *kernelCrashHelper {
	t.Helper()
	readyR, readyW, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	holdR, holdW, err := os.Pipe()
	if err != nil {
		readyR.Close()
		readyW.Close()
		t.Fatal(err)
	}
	command := exec.Command(os.Args[0], "-test.run=^TestKernelConcreteCrashHelper$")
	command.Env = append(os.Environ(),
		kernelCrashModeEnv+"="+mode,
		kernelCrashPathEnv+"="+path,
		kernelCrashRunEnv+"="+hex.EncodeToString(runID.Bytes()),
		kernelCrashRevisionEnv+"="+strconv.FormatInt(revision.Int64(), 10),
	)
	command.ExtraFiles = []*os.File{readyW, holdR}
	helper := &kernelCrashHelper{command: command, ready: readyR, hold: holdW}
	command.Stderr = &helper.stderr
	if err := command.Start(); err != nil {
		readyR.Close()
		readyW.Close()
		holdR.Close()
		holdW.Close()
		t.Fatal(err)
	}
	readyW.Close()
	holdR.Close()
	t.Cleanup(func() {
		if helper.command.Process != nil {
			_ = helper.command.Process.Kill()
			_, _ = helper.command.Process.Wait()
		}
		_ = helper.ready.Close()
		_ = helper.hold.Close()
	})
	return helper
}

func (helper *kernelCrashHelper) waitReady(t *testing.T) {
	t.Helper()
	if err := helper.ready.SetReadDeadline(time.Now().Add(10 * time.Second)); err != nil {
		t.Fatal(err)
	}
	var signal [1]byte
	if _, err := io.ReadFull(helper.ready, signal[:]); err != nil {
		t.Fatalf("crash helper readiness: %v; stderr=%s", err, helper.stderr.String())
	}
}

func (helper *kernelCrashHelper) killAndWait(t *testing.T) {
	t.Helper()
	if err := helper.command.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	err := helper.command.Wait()
	exit, ok := err.(*exec.ExitError)
	if !ok {
		t.Fatalf("helper wait = %v; stderr=%s", err, helper.stderr.String())
	}
	status, ok := exit.Sys().(syscall.WaitStatus)
	if !ok || !status.Signaled() || status.Signal() != syscall.SIGKILL {
		t.Fatalf("helper status = %#v, want SIGKILL; stderr=%s", exit.Sys(), helper.stderr.String())
	}
	helper.command.Process = nil
}

func TestConcreteStoreCrashBeforeAndAfterAdmissionCommit(t *testing.T) {
	for _, test := range []struct {
		name         string
		mode         string
		wantRevision int64
		wantHead     int64
		// A committed admission also publishes the project's incremented lifetime
		// run count, so its project invalidation advances the durable head once.
	}{{"before", "admit-before", 1, 3}, {"after", "admit-after", 2, 7}} {
		t.Run(test.name, func(t *testing.T) {
			store, path, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
			task, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 228), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 229), Title: "crash"}, mustTime(t, 5))
			if err != nil {
				t.Fatal(err)
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			helper := startKernelCrashHelper(t, test.mode, path, RunID{}, Revision{})
			helper.waitReady(t)
			helper.killAndWait(t)
			reopened, err := Open(context.Background(), path)
			if err != nil {
				t.Fatal(err)
			}
			defer reopened.Close()
			state, err := reopened.Factory(context.Background())
			if err != nil {
				t.Fatal(err)
			}
			if state.Revision.Int64() != test.wantRevision || state.Head.Int64() != test.wantHead || !state.DispatchEnabled {
				t.Fatalf("crash footprint = %+v", state)
			}
			keys := admissionKeys(t, 230, nil)
			admission, reconcileErr := reopened.ReconcileAdmission(context.Background(), keys)
			freshTask, found, taskErr := reopened.Task(context.Background(), task.ID)
			if taskErr != nil || !found {
				t.Fatal(taskErr)
			}
			if test.wantRevision == 1 {
				if reconcileErr != nil || admission.Admitted() || admission.Reason != NoAdmissionNotReconciled || freshTask.Status != TaskQueued {
					t.Fatalf("pre-commit reconciliation = %+v err=%v task=%+v", admission, reconcileErr, freshTask)
				}
			} else if reconcileErr != nil || !admission.Admitted() || freshTask.Status != TaskRunning {
				t.Fatalf("post-commit reconciliation = %+v err=%v task=%+v", admission, reconcileErr, freshTask)
			}
		})
	}
}

func TestConcreteStoreCrashBeforeAndAfterFinalizationCommit(t *testing.T) {
	for _, test := range []struct {
		name      string
		mode      string
		wantPhase RunPhase
	}{{"before", "finalize-before", RunFinalizing}, {"after", "finalize-after", RunTerminal}} {
		t.Run(test.name, func(t *testing.T) {
			store, run, _ := runningOrchestratorRun(t)
			path := storePath(t, store)
			proposal, _ := NewSuccessProposal("done")
			finalizing, err := store.ProposeAttemptOutcome(context.Background(), run.CredentialDigest, proposal, mustTime(t, 40))
			if err != nil {
				t.Fatal(err)
			}
			finalizing = observeMissingProcessExits(t, store, run.ID, 41)
			for index, resource := range resourcesForRunTest(t, store, run.ID) {
				if resource.State == ResourceReleased {
					continue
				}
				if _, err := store.ReleaseResource(context.Background(), run.ID, resource.ID, resource.Revision, resource.Identity, mustTime(t, 50+int64(index))); err != nil {
					t.Fatal(err)
				}
			}
			finalizing = closeTerminalSessionAtCurrent(t, store, run.ID, 54)
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			helper := startKernelCrashHelper(t, test.mode, path, run.ID, finalizing.Revision)
			helper.waitReady(t)
			helper.killAndWait(t)
			reopened, err := Open(context.Background(), path)
			if err != nil {
				t.Fatal(err)
			}
			defer reopened.Close()
			fresh, found, err := reopened.Run(context.Background(), run.ID)
			if err != nil || !found || fresh.Phase != test.wantPhase {
				t.Fatalf("run after crash = %+v found=%v err=%v", fresh, found, err)
			}
			task, found, err := reopened.Task(context.Background(), run.TaskID)
			if err != nil || !found {
				t.Fatal(err)
			}
			wantTask := TaskRunning
			if test.wantPhase == RunTerminal {
				wantTask = TaskSucceeded
			}
			if task.Status != wantTask {
				t.Fatalf("task after crash = %+v, want %s", task, wantTask.String())
			}
		})
	}
}
