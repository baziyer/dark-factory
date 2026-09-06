//go:build darwin

package changeworker

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

type runtimeAuthority struct {
	root, namedRoot, home, temp, token *os.File
	runtimePath                        string
	rootID                             runner.FileIdentity
	homeID, tempID, tokenID            runner.FileIdentity
}

const providerTaskName = ".provider-task"

func runProvider(ctx context.Context) (resultErr error) {
	control, err := runner.OpenWorkerControl()
	if err != nil {
		return err
	}
	defer func() {
		if resultErr != nil {
			// The worker may fail during the final source/authority scan or task
			// sealing. Preserve that exact bounded cause for the outer owner
			// instead of reducing it to an unexplained EOF.
			_ = control.ReportProviderError(resultErr)
		}
		resultErr = errors.Join(resultErr, control.Close())
	}()
	encodedConfig, err := control.ReadConfig(ConfigLimit)
	if err != nil {
		return err
	}
	config, err := DecodeConfig(encodedConfig)
	if err != nil {
		return err
	}
	runtimeDir, err := control.DuplicateRuntimeDirectory(ctx)
	if err != nil {
		return err
	}
	authority, err := openRuntimeAuthority(ctx, runtimeDir, config)
	if err != nil {
		_ = runtimeDir.Close()
		return err
	}
	authority.root = runtimeDir
	defer func() { resultErr = errors.Join(resultErr, authority.close()) }()
	factoryctl, err := runner.CommitExecutableLocator(config.FactoryctlExecutable)
	if err != nil {
		return err
	}

	var verified *change.VerifiedPublished
	publishedPath := filepath.Join(config.ChangeParent, config.FinalName)
	if config.Retained == nil {
		verified, err = prepareFreshChange(ctx, control, config)
	} else {
		verified, err = openRetainedChange(ctx, control, config)
	}
	if err != nil {
		return err
	}
	verifiedOpen := true
	defer func() {
		if verifiedOpen {
			resultErr = errors.Join(resultErr, verified.Close())
		}
	}()
	if err := authority.verify(ctx); err != nil {
		return fmt.Errorf("runtime authority verification: %w", err)
	}
	cwd, err := verified.DuplicateDirectory(ctx)
	if err != nil {
		return err
	}
	if err := verified.Close(); err != nil {
		verifiedOpen = false
		_ = cwd.Close()
		return err
	}
	verifiedOpen = false
	if err := authority.verify(ctx); err != nil {
		_ = cwd.Close()
		return fmt.Errorf("runtime authority verification: %w", err)
	}

	home := filepath.Join(config.RuntimePath, HomeName)
	temp := filepath.Join(config.RuntimePath, TempName)
	token := filepath.Join(config.RuntimePath, AttemptTokenName)
	runtimePaths, err := provider.NewRuntimePaths(
		home, temp, config.AttemptSocket, token, factoryctl.Path(), filepath.Dir(publishedPath), config.ToolPath, config.AccountHome, config.AccountConfigDir,
	)
	if err != nil {
		_ = cwd.Close()
		return err
	}
	installation, err := provider.ResolveInstallation(config.Provider, config.ToolPath)
	if err != nil {
		_ = cwd.Close()
		return err
	}
	// Keep the one verified publication path as the authority for both Codex's
	// project policy and the runner's process cwd below.
	request, err := provider.NewRequest(config.Provider, installation, config.Model, config.ReasoningEffort, runtimePaths, publishedPath)
	if err != nil {
		_ = cwd.Close()
		return err
	}
	launch, err := provider.Build(request)
	if err != nil {
		_ = cwd.Close()
		return err
	}
	delivery, program, err := prepareProviderTask(config.Provider, config.ProviderTask)
	if err != nil {
		_ = cwd.Close()
		return err
	}
	if delivery != launch.TaskDelivery() {
		_ = cwd.Close()
		return provider.ErrInvalid
	}
	spec, err := runner.PrepareCommittedExecSpec(launch.Executable(), launch.Argv(), launch.Environment(), publishedPath)
	if err != nil {
		_ = cwd.Close()
		return err
	}
	if err := authority.verify(ctx); err != nil {
		_ = cwd.Close()
		return fmt.Errorf("runtime authority verification: %w", err)
	}
	if err := factoryctl.Verify(); err != nil {
		_ = cwd.Close()
		return err
	}
	if err := launch.Executable().Verify(); err != nil {
		_ = cwd.Close()
		return err
	}
	var task *os.File
	if delivery == provider.TaskDeliveryFD11 {
		task, err = authority.sealProviderTask(config.Provider, program)
		if err != nil {
			_ = cwd.Close()
			return err
		}
	} else if delivery != provider.TaskDeliveryStartupTerminal && delivery != provider.TaskDeliveryAttemptAPI {
		_ = cwd.Close()
		return provider.ErrInvalid
	}
	clear(program)
	taskOpen := task != nil
	defer func() {
		if task != nil && taskOpen {
			resultErr = errors.Join(resultErr, task.Close())
		}
	}()
	// Retain the descriptor-bound runtime authority until exec. Its members are
	// CLOEXEC, so a successful provider image receives none of them; a failed
	// exec returns through the ordinary defer and closes them. Shell's task is
	// unlinked and read-only; the attempt runner separately owns Claude startup,
	// while Codex receives no task-bearing descriptor.
	if err := authority.verify(ctx); err != nil {
		_ = cwd.Close()
		return fmt.Errorf("runtime authority verification: %w", err)
	}
	// The one effect outside the runtime before exec, so it is the last thing
	// before it: nothing after this can refuse the launch and leave a record
	// for a Change that never ran.
	if config.Provider == kernel.ProviderClaudeCode {
		if err := provider.TrustClaudeDirectory(runtimePaths, publishedPath); err != nil {
			_ = cwd.Close()
			return err
		}
	}
	taskOpen = false
	return control.ExecProvider(spec, cwd, task)
}

// sealProviderTask turns the admission-owned bytes into one exact, unlinked,
// read-only descriptor. The temporary name exists only inside the already
// registered private runtime and is removed before the descriptor crosses the
// runner boundary. No pathname is carried into provider exec.
func (a *runtimeAuthority) sealProviderTask(kind kernel.Provider, task []byte) (_ *os.File, resultErr error) {
	if _, _, err := provider.PrepareTask(kind, task); a == nil || a.temp == nil || err != nil {
		return nil, ErrWorker
	}
	tempID, err := privateDirectory(a.temp)
	if err != nil || tempID != a.tempID {
		return nil, ErrWorker
	}
	fd, err := unix.Openat(int(a.temp.Fd()), providerTaskName, unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0o600)
	if err != nil {
		return nil, ErrWorker
	}
	writer := os.NewFile(uintptr(fd), "provider-task-writer")
	var created runner.FileIdentity
	named := false
	var reader *os.File
	defer func() {
		if writer != nil {
			resultErr = errors.Join(resultErr, writer.Close())
		}
		if resultErr != nil && reader != nil {
			resultErr = errors.Join(resultErr, reader.Close())
		}
		if named && created.Device != 0 && created.Inode != 0 {
			var current unix.Stat_t
			if unix.Fstatat(int(a.temp.Fd()), providerTaskName, &current, unix.AT_SYMLINK_NOFOLLOW) == nil && uint64(current.Dev) == created.Device && current.Ino == created.Inode {
				resultErr = errors.Join(resultErr, unix.Unlinkat(int(a.temp.Fd()), providerTaskName, 0))
				resultErr = errors.Join(resultErr, unix.Fsync(int(a.temp.Fd())))
			}
		}
	}()
	var empty unix.Stat_t
	if err := unix.Fstat(fd, &empty); err != nil || empty.Mode&unix.S_IFMT != unix.S_IFREG || empty.Uid != uint32(os.Geteuid()) || empty.Mode&0o7777 != 0o600 || empty.Nlink != 1 || empty.Size != 0 || empty.Dev == 0 || empty.Ino == 0 || uint64(empty.Dev) != a.tempID.Device {
		return nil, ErrWorker
	}
	created = runner.FileIdentity{Device: uint64(empty.Dev), Inode: empty.Ino}
	named = true
	n, err := writer.Write(task)
	if err != nil || n != len(task) || writer.Sync() != nil {
		return nil, ErrWorker
	}
	readFD, err := unix.Openat(int(a.temp.Fd()), providerTaskName, unix.O_RDONLY|unix.O_NONBLOCK|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return nil, ErrWorker
	}
	reader = os.NewFile(uintptr(readFD), "provider-task")
	var sealed unix.Stat_t
	if err := unix.Fstat(readFD, &sealed); err != nil || sealed.Mode&unix.S_IFMT != unix.S_IFREG || sealed.Uid != uint32(os.Geteuid()) || sealed.Mode&0o7777 != 0o600 || sealed.Nlink != 1 || sealed.Size != int64(len(task)) || uint64(sealed.Dev) != created.Device || sealed.Ino != created.Inode {
		return nil, ErrWorker
	}
	flags, err := unix.FcntlInt(reader.Fd(), unix.F_GETFL, 0)
	if err != nil || flags&unix.O_ACCMODE != unix.O_RDONLY {
		return nil, ErrWorker
	}
	body, err := io.ReadAll(io.LimitReader(reader, int64(runner.MaxProviderTaskBytes)+1))
	if err != nil || !bytes.Equal(body, task) {
		return nil, ErrWorker
	}
	if _, err := reader.Seek(0, io.SeekStart); err != nil {
		return nil, ErrWorker
	}
	var namedStat unix.Stat_t
	if err := unix.Fstatat(int(a.temp.Fd()), providerTaskName, &namedStat, unix.AT_SYMLINK_NOFOLLOW); err != nil || uint64(namedStat.Dev) != created.Device || namedStat.Ino != created.Inode {
		return nil, ErrWorker
	}
	if err := unix.Unlinkat(int(a.temp.Fd()), providerTaskName, 0); err != nil {
		return nil, ErrWorker
	}
	named = false
	if err := unix.Fsync(int(a.temp.Fd())); err != nil {
		return nil, ErrWorker
	}
	if err := writer.Close(); err != nil {
		writer = nil
		return nil, ErrWorker
	}
	writer = nil
	var unlinked unix.Stat_t
	if err := unix.Fstat(readFD, &unlinked); err != nil || unlinked.Nlink != 0 || unlinked.Size != int64(len(task)) || uint64(unlinked.Dev) != created.Device || unlinked.Ino != created.Inode {
		return nil, ErrWorker
	}
	tempID, err = privateDirectory(a.temp)
	if err != nil || tempID != a.tempID {
		return nil, ErrWorker
	}
	return reader, nil
}

func prepareFreshChange(ctx context.Context, control *runner.WorkerControl, config Config) (_ *change.VerifiedPublished, resultErr error) {
	selection, err := change.SelectGit(ctx, config.GitExecutable, config.RepositoryRoot, config.Revision, config.RepositoryIdentity)
	if err != nil {
		return nil, err
	}
	if !selection.RepositoryIdentity().Equal(config.RepositoryIdentity) || control.ReportSelection(nil) != nil {
		return nil, ErrWorker
	}
	if err := control.AwaitPreparation(); err != nil {
		return nil, err
	}
	prepared, err := change.Prepare(ctx, config.ChangeParent, config.FinalName, config.StagingName)
	if err != nil {
		return nil, err
	}
	preparedOpen := true
	defer func() {
		if preparedOpen {
			resultErr = errors.Join(resultErr, prepared.Close())
		}
	}()
	preparationBytes, err := EncodeResult(Result{
		Format: selection.ObjectFormat(), Base: selection.Base(), Commitment: selection.Commitment(),
		EntryCount: selection.EntryCount(), BlobBytes: selection.BlobBytes(), Tree: prepared.Identity(),
	})
	if err != nil || control.ReportPreparation(preparationBytes) != nil {
		return nil, ErrWorker
	}
	if err := control.AwaitPopulation(); err != nil {
		return nil, err
	}
	blobs, err := change.OpenGitBlobs(ctx, config.GitExecutable, config.RepositoryRoot, selection)
	if err != nil {
		return nil, err
	}
	blobsOpen := true
	defer func() {
		if blobsOpen {
			resultErr = errors.Join(resultErr, blobs.Abort())
		}
	}()
	published, err := prepared.PopulateAndPublish(ctx, selection.Manifest(), blobs.Read)
	if err != nil {
		return nil, err
	}
	if err := blobs.Close(); err != nil {
		blobsOpen = false
		return nil, err
	}
	blobsOpen = false
	if err := prepared.Close(); err != nil {
		preparedOpen = false
		return nil, err
	}
	preparedOpen = false
	if err := reportPopulation(control); err != nil {
		return nil, err
	}
	if err := control.AwaitProvider(); err != nil {
		return nil, err
	}
	return published.Reinspect(ctx)
}

func openRetainedChange(ctx context.Context, control *runner.WorkerControl, config Config) (*change.VerifiedPublished, error) {
	retained := config.Retained
	if retained == nil || change.VerifyRepositoryRoot(config.RepositoryRoot, config.RepositoryIdentity) != nil {
		return nil, ErrWorker
	}
	if control.ReportSelection(nil) != nil {
		return nil, ErrWorker
	}
	if err := control.AwaitPreparation(); err != nil {
		return nil, err
	}
	preparationBytes, err := EncodeResult(*retained)
	if err != nil || control.ReportPreparation(preparationBytes) != nil {
		return nil, ErrWorker
	}
	if err := control.AwaitPopulation(); err != nil {
		return nil, err
	}
	facts, err := change.InspectPublished(ctx, config.ChangeParent, config.FinalName, retained.Tree, retained.Format, retained.Base)
	if err != nil || !retainedFactsEqual(*retained, facts) {
		return nil, errors.Join(err, ErrWorker)
	}
	if err := reportPopulation(control); err != nil {
		return nil, err
	}
	if err := control.AwaitProvider(); err != nil {
		return nil, err
	}
	if err := change.VerifyRepositoryRoot(config.RepositoryRoot, config.RepositoryIdentity); err != nil {
		return nil, err
	}
	verified, err := change.OpenPublished(ctx, config.ChangeParent, config.FinalName, retained.Tree, retained.Format, retained.Base)
	if err != nil {
		return nil, err
	}
	verifiedFacts, err := verified.Facts()
	if err != nil || !retainedFactsEqual(*retained, verifiedFacts) {
		_ = verified.Close()
		return nil, errors.Join(err, ErrWorker)
	}
	return verified, nil
}

func reportPopulation(control *runner.WorkerControl) error {
	group := runner.ObserveProcessGroup(control.Identity())
	if group.Presence != runner.Present || len(group.Members) != 1 || group.Members[0] != control.Identity() {
		return ErrWorker
	}
	if control.ReportPopulation(nil) != nil {
		return ErrWorker
	}
	return nil
}

func retainedFactsEqual(retained Result, facts change.TreeFacts) bool {
	return facts.Identity().Equal(retained.Tree) && facts.Commitment().Equal(retained.Commitment) && facts.EntryCount() == retained.EntryCount && facts.BlobBytes() == retained.BlobBytes
}

func openRuntimeAuthority(ctx context.Context, runtimeDir *os.File, config Config) (*runtimeAuthority, error) {
	if runtimeDir == nil || ctx.Err() != nil {
		return nil, ErrWorker
	}
	rootID, err := privateDirectory(runtimeDir)
	if err != nil {
		return nil, err
	}
	if config.RuntimeIdentity != rootID {
		return nil, ErrWorker
	}
	namedRoot, err := openCanonicalDirectory(config.RuntimePath)
	if err != nil {
		return nil, err
	}
	namedID, err := privateDirectory(namedRoot)
	if err != nil || namedID != rootID {
		_ = namedRoot.Close()
		return nil, ErrWorker
	}
	home, homeID, err := openPrivateDirectoryAt(int(runtimeDir.Fd()), HomeName, rootID.Device)
	if err != nil {
		_ = namedRoot.Close()
		return nil, err
	}
	temp, tempID, err := openPrivateDirectoryAt(int(runtimeDir.Fd()), TempName, rootID.Device)
	if err != nil {
		_ = home.Close()
		_ = namedRoot.Close()
		return nil, err
	}
	token, tokenID, _, body, err := openPrivateFile(int(runtimeDir.Fd()), AttemptTokenName, 32, rootID.Device)
	if err != nil || len(body) != 32 {
		if token != nil {
			_ = token.Close()
		}
		_ = temp.Close()
		_ = home.Close()
		_ = namedRoot.Close()
		return nil, ErrWorker
	}
	return &runtimeAuthority{runtimePath: config.RuntimePath, namedRoot: namedRoot, home: home, temp: temp, token: token, rootID: rootID, homeID: homeID, tempID: tempID, tokenID: tokenID}, nil
}

func (a *runtimeAuthority) verify(ctx context.Context) error {
	if a == nil || ctx.Err() != nil {
		if ctx != nil {
			return ctx.Err()
		}
		return ErrWorker
	}
	rootID, err := privateDirectory(a.root)
	if err != nil || rootID != a.rootID {
		return ErrWorker
	}
	namedID, err := privateDirectory(a.namedRoot)
	if err != nil || namedID != a.rootID {
		return ErrWorker
	}
	reopened, err := openCanonicalDirectory(a.runtimePath)
	if err != nil {
		return ErrWorker
	}
	reopenedID, reopenedErr := privateDirectory(reopened)
	closeErr := reopened.Close()
	if reopenedErr != nil || closeErr != nil || reopenedID != a.rootID {
		return ErrWorker
	}
	for _, item := range []struct {
		file *os.File
		id   runner.FileIdentity
	}{{a.home, a.homeID}, {a.temp, a.tempID}} {
		got, err := privateDirectory(item.file)
		if err != nil || got != item.id || runtimeDevice(got, a.rootID.Device) != nil {
			return ErrWorker
		}
	}
	var token unix.Stat_t
	if err := unix.Fstat(int(a.token.Fd()), &token); err != nil || token.Mode&unix.S_IFMT != unix.S_IFREG || token.Uid != uint32(os.Geteuid()) || token.Mode&0o7777 != 0o600 || token.Nlink != 1 || token.Size != 32 {
		return ErrWorker
	}
	if (runner.FileIdentity{Device: uint64(token.Dev), Inode: token.Ino}) != a.tokenID || runtimeDevice(a.tokenID, a.rootID.Device) != nil {
		return ErrWorker
	}
	for _, item := range []struct {
		name string
		id   runner.FileIdentity
	}{{HomeName, a.homeID}, {TempName, a.tempID}, {AttemptTokenName, a.tokenID}} {
		var stat unix.Stat_t
		if err := unix.Fstatat(int(a.root.Fd()), item.name, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil || (runner.FileIdentity{Device: uint64(stat.Dev), Inode: stat.Ino}) != item.id {
			return ErrWorker
		}
	}
	return ctx.Err()
}

func (a *runtimeAuthority) close() error {
	if a == nil {
		return nil
	}
	var err error
	for _, file := range []*os.File{a.token, a.temp, a.home, a.namedRoot, a.root} {
		if file != nil {
			err = errors.Join(err, file.Close())
		}
	}
	a.root, a.namedRoot, a.home, a.temp, a.token = nil, nil, nil, nil, nil
	return err
}

func privateDirectory(file *os.File) (runner.FileIdentity, error) {
	if file == nil {
		return runner.FileIdentity{}, ErrWorker
	}
	var stat unix.Stat_t
	if err := unix.Fstat(int(file.Fd()), &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Uid != uint32(os.Geteuid()) || stat.Mode&0o7777 != 0o700 || stat.Dev == 0 || stat.Ino == 0 {
		return runner.FileIdentity{}, ErrWorker
	}
	return runner.FileIdentity{Device: uint64(stat.Dev), Inode: stat.Ino}, nil
}

func openPrivateDirectoryAt(parent int, name string, rootDevice uint64) (*os.File, runner.FileIdentity, error) {
	fd, err := unix.Openat(parent, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return nil, runner.FileIdentity{}, ErrWorker
	}
	file := os.NewFile(uintptr(fd), "private-runtime-directory")
	identity, err := privateDirectory(file)
	if err != nil || runtimeDevice(identity, rootDevice) != nil {
		_ = file.Close()
		return nil, runner.FileIdentity{}, ErrWorker
	}
	return file, identity, nil
}

func openPrivateFile(parent int, name string, maximum int, rootDevice uint64) (*os.File, runner.FileIdentity, int64, []byte, error) {
	fd, err := unix.Openat(parent, name, unix.O_RDONLY|unix.O_NONBLOCK|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return nil, runner.FileIdentity{}, 0, nil, ErrWorker
	}
	file := os.NewFile(uintptr(fd), "private-runtime-file")
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFREG || stat.Uid != uint32(os.Geteuid()) || stat.Mode&0o7777 != 0o600 || stat.Nlink != 1 || stat.Size <= 0 || stat.Size > int64(maximum) {
		_ = file.Close()
		return nil, runner.FileIdentity{}, 0, nil, ErrWorker
	}
	body, err := io.ReadAll(io.LimitReader(file, int64(maximum)+1))
	if err != nil || int64(len(body)) != stat.Size {
		_ = file.Close()
		return nil, runner.FileIdentity{}, 0, nil, ErrWorker
	}
	id := runner.FileIdentity{Device: uint64(stat.Dev), Inode: stat.Ino}
	if err := verifyOpenPrivateFile(file, id, stat.Size); err != nil || runtimeDevice(id, rootDevice) != nil {
		_ = file.Close()
		return nil, runner.FileIdentity{}, 0, nil, ErrWorker
	}
	return file, id, stat.Size, body, nil
}

func runtimeDevice(identity runner.FileIdentity, rootDevice uint64) error {
	if rootDevice == 0 || identity.Device != rootDevice {
		return ErrWorker
	}
	return nil
}

func verifyOpenPrivateFile(file *os.File, id runner.FileIdentity, size int64) error {
	var stat unix.Stat_t
	if file == nil || unix.Fstat(int(file.Fd()), &stat) != nil || uint64(stat.Dev) != id.Device || stat.Ino != id.Inode || stat.Mode&unix.S_IFMT != unix.S_IFREG || stat.Uid != uint32(os.Geteuid()) || stat.Mode&0o7777 != 0o600 || stat.Nlink != 1 || stat.Size != size {
		return ErrWorker
	}
	return nil
}

func openCanonicalDirectory(path string) (*os.File, error) {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path || path == "/" || !strings.HasPrefix(path, "/") {
		return nil, ErrWorker
	}
	fd, err := unix.Open("/", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, ErrWorker
	}
	current := os.NewFile(uintptr(fd), "runtime-path-root")
	for _, component := range strings.Split(strings.TrimPrefix(path, "/"), "/") {
		nextFD, openErr := unix.Openat(int(current.Fd()), component, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
		_ = current.Close()
		if openErr != nil {
			return nil, ErrWorker
		}
		current = os.NewFile(uintptr(nextFD), "runtime-path-component")
	}
	return current, nil
}
