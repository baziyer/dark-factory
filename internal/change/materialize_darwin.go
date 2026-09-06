//go:build darwin

package change

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

const directoryMode = 0o700

// Prepared is one create-only declared staging directory whose exact identity
// remains open while the caller records it. It is single-use.
type Prepared struct {
	mu sync.Mutex

	parent   string
	target   string
	staging  string
	parentFD int
	stageFD  int
	parentID StageIdentity
	identity StageIdentity
	consumed bool
	closed   bool
}

// Prepare creates exactly staging below parent and retains verified parent and
// staging descriptors. The caller records Identity before invoking
// PopulateAndPublish; Store tests own proof that this record is durable.
func Prepare(ctx context.Context, parent, target, staging string) (*Prepared, error) {
	return prepare(ctx, parent, target, staging, nil)
}

// AdoptPrepared opens one caller-declared staging directory retained after a
// crash before its identity was recorded. It accepts only the exact empty,
// private stage and never mutates or removes either declared path.
func AdoptPrepared(ctx context.Context, parent, target, staging string) (*Prepared, error) {
	return adoptPrepared(ctx, parent, target, staging, nil)
}

func adoptPrepared(ctx context.Context, parent, target, staging string, hook materializeHook) (*Prepared, error) {
	if err := validatePrepareLocator(parent, target, staging); err != nil {
		return nil, err
	}
	if err := checkpoint(ctx, hook, materializePoint{}); err != nil {
		return nil, err
	}
	parentFD, err := openVerifiedParent(parent)
	if err != nil {
		return nil, err
	}
	closeParent := true
	defer func() {
		if closeParent {
			_ = unix.Close(parentFD)
		}
	}()
	parentID, err := verifySecureParent(parentFD)
	if err != nil {
		return nil, err
	}
	if err := verifyParentPath(parent, parentID); err != nil {
		return nil, err
	}
	if err := requireNamedAbsent(parentFD, parent, target); err != nil {
		return nil, err
	}

	identity, err := namedIdentity(parentFD, staging)
	if errors.Is(err, unix.ENOENT) {
		return nil, &ValidationError{Reason: "declared staging directory does not exist"}
	}
	if err != nil {
		return nil, unresolved("identify retained staging", parent, staging, StageIdentity{}, false, err)
	}
	if !identity.valid() {
		return nil, unresolved("represent retained staging identity", parent, staging, identity, true, &ValidationError{Reason: "stage identity is not representable in Store"})
	}
	point := materializePoint{step: stepBeforeAdoptOpen, parent: parent, stagingName: staging, targetName: target}
	if err := checkpoint(ctx, hook, point); err != nil {
		return nil, unresolved("open retained staging", parent, staging, identity, true, err)
	}
	stageFD, err := unix.Openat(parentFD, staging, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return nil, unresolved("open retained staging", parent, staging, identity, true, err)
	}
	closeStage := true
	defer func() {
		if closeStage {
			_ = unix.Close(stageFD)
		}
	}()
	point.step = stepAfterAdoptOpen
	if err := checkpoint(ctx, hook, point); err != nil {
		return nil, unresolved("validate opened retained staging", parent, staging, identity, true, err)
	}
	if err := verifyAdoptableStage(stageFD, identity); err != nil {
		return nil, unresolved("verify opened retained staging", parent, staging, identity, true, err)
	}
	if err := verifyNamedAdoptableStage(parentFD, staging, identity); err != nil {
		return nil, unresolved("bind retained staging name", parent, staging, identity, true, err)
	}
	if err := verifyParentFD(parentFD, parentID); err != nil {
		return nil, unresolved("verify retained staging parent", parent, staging, identity, true, err)
	}
	if err := verifyParentPath(parent, parentID); err != nil {
		return nil, unresolved("bind retained staging parent", parent, staging, identity, true, err)
	}
	hasEntry, err := directoryHasEntry(ctx, stageFD)
	if err != nil {
		return nil, unresolved("inspect retained staging contents", parent, staging, identity, true, err)
	}
	if hasEntry {
		return nil, unresolved("inspect retained staging contents", parent, staging, identity, true, &ValidationError{Reason: "retained staging directory is not empty"})
	}
	if err := verifyAdoptableStage(stageFD, identity); err != nil {
		return nil, unresolved("reverify opened retained staging", parent, staging, identity, true, err)
	}
	if err := verifyNamedAdoptableStage(parentFD, staging, identity); err != nil {
		return nil, unresolved("rebind retained staging name", parent, staging, identity, true, err)
	}
	if err := verifyParentPath(parent, parentID); err != nil {
		return nil, unresolved("rebind retained staging parent", parent, staging, identity, true, err)
	}
	if err := requireNamedAbsent(parentFD, parent, target); err != nil {
		return nil, unresolved("recheck absent Change target", parent, staging, identity, true, err)
	}
	closeParent = false
	closeStage = false
	return newPrepared(parent, target, staging, parentFD, stageFD, parentID, identity), nil
}

func prepare(ctx context.Context, parent, target, staging string, hook materializeHook) (*Prepared, error) {
	if err := validatePrepareLocator(parent, target, staging); err != nil {
		return nil, err
	}
	if err := checkpoint(ctx, hook, materializePoint{}); err != nil {
		return nil, err
	}
	parentFD, err := openVerifiedParent(parent)
	if err != nil {
		return nil, err
	}
	parentID, err := verifySecureParent(parentFD)
	if err != nil {
		unix.Close(parentFD)
		return nil, err
	}
	if err := verifyParentPath(parent, parentID); err != nil {
		unix.Close(parentFD)
		return nil, err
	}
	for _, name := range []string{target, staging} {
		if exists, err := namedEntryExists(parentFD, name); err != nil {
			unix.Close(parentFD)
			return nil, err
		} else if exists {
			unix.Close(parentFD)
			return nil, &ConflictError{Target: changePath(parent, name)}
		}
	}
	if err := unix.Mkdirat(parentFD, staging, directoryMode); err != nil {
		unix.Close(parentFD)
		if errors.Is(err, unix.EEXIST) {
			return nil, &ConflictError{Target: changePath(parent, staging)}
		}
		return nil, fmt.Errorf("create declared Change staging directory: %w", err)
	}

	identity, identityErr := namedIdentity(parentFD, staging)
	if identityErr != nil {
		unix.Close(parentFD)
		return nil, unresolved("identify declared staging after mkdir", parent, staging, StageIdentity{}, false, identityErr)
	}
	if !identity.valid() {
		unix.Close(parentFD)
		return nil, unresolved("represent declared staging identity", parent, staging, identity, true, &ValidationError{Reason: "stage identity is not representable in Store"})
	}
	point := materializePoint{step: stepAfterPrepareMkdir, parent: parent, stagingName: staging, targetName: target}
	if err := checkpoint(ctx, hook, point); err != nil {
		unix.Close(parentFD)
		return nil, unresolved("validate declared staging after mkdir", parent, staging, identity, true, err)
	}
	stageFD, err := unix.Openat(parentFD, staging, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		unix.Close(parentFD)
		return nil, unresolved("open declared staging", parent, staging, identity, true, err)
	}
	if err := verifyOpenIdentity(stageFD, identity); err != nil {
		unix.Close(stageFD)
		unix.Close(parentFD)
		return nil, unresolved("bind opened declared staging", parent, staging, identity, true, err)
	}
	if err := unix.Fchmod(stageFD, directoryMode); err != nil {
		unix.Close(stageFD)
		unix.Close(parentFD)
		return nil, unresolved("secure declared staging", parent, staging, identity, true, err)
	}
	if err := verifyOpenRoot(stageFD, identity); err != nil {
		unix.Close(stageFD)
		unix.Close(parentFD)
		return nil, unresolved("verify declared staging", parent, staging, identity, true, err)
	}
	if err := verifyNamedRoot(parentFD, staging, identity); err != nil {
		unix.Close(stageFD)
		unix.Close(parentFD)
		return nil, unresolved("bind declared staging name", parent, staging, identity, true, err)
	}
	point.step = stepBeforePrepareFsync
	if err := checkpoint(ctx, hook, point); err != nil {
		unix.Close(stageFD)
		unix.Close(parentFD)
		return nil, unresolved("sync declared staging", parent, staging, identity, true, err)
	}
	if err := unix.Fsync(stageFD); err != nil {
		unix.Close(stageFD)
		unix.Close(parentFD)
		return nil, unresolved("fsync declared staging", parent, staging, identity, true, err)
	}
	if err := unix.Fsync(parentFD); err != nil {
		unix.Close(stageFD)
		unix.Close(parentFD)
		return nil, unresolved("fsync declared staging parent", parent, staging, identity, true, err)
	}
	return newPrepared(parent, target, staging, parentFD, stageFD, parentID, identity), nil
}

func newPrepared(parent, target, staging string, parentFD, stageFD int, parentID, identity StageIdentity) *Prepared {
	return &Prepared{
		parent: parent, target: target, staging: staging,
		parentFD: parentFD, stageFD: stageFD, parentID: parentID, identity: identity,
	}
}

// Identity returns the exact retained staging root identity.
func (p *Prepared) Identity() StageIdentity {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.identity
}

// PopulateAndPublish consumes this handle exactly once, reconstructs and syncs
// the complete tree, then publishes with Darwin atomic no-replace rename.
func (p *Prepared) PopulateAndPublish(ctx context.Context, manifest Manifest, source BlobSource) (Published, error) {
	return p.populateAndPublish(ctx, manifest, source, nil)
}

func (p *Prepared) populateAndPublish(ctx context.Context, manifest Manifest, source BlobSource, hook materializeHook) (Published, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed {
		return Published{}, &LifecycleError{Reason: "populate after close"}
	}
	if p.consumed {
		return Published{}, &LifecycleError{Reason: "populate more than once"}
	}
	p.consumed = true
	if source == nil || !manifest.format.valid() || !manifest.base.format.valid() || manifest.base.format != manifest.format {
		return Published{}, &ValidationError{Reason: "valid manifest and blob source are required"}
	}
	if err := checkpoint(ctx, hook, materializePoint{}); err != nil {
		return Published{}, err
	}
	if err := p.verifyPreparedRoot(); err != nil {
		return Published{}, err
	}
	hasEntry, err := directoryHasEntry(ctx, p.stageFD)
	if err != nil {
		return Published{}, err
	}
	if hasEntry {
		return Published{}, &ValidationError{Reason: "prepared staging directory is not empty"}
	}

	for _, entry := range manifest.entries {
		if err := p.materializeEntry(ctx, entry, source, hook); err != nil {
			return Published{}, err
		}
	}
	point := p.point(stepBeforeTreeVerify, nil)
	if err := checkpoint(ctx, hook, point); err != nil {
		return Published{}, err
	}
	point.step = stepBeforeTreeFsync
	if err := checkpoint(ctx, hook, point); err != nil {
		return Published{}, err
	}
	actual, err := scanTree(ctx, p.stageFD, p.identity.device, manifest.format, manifest.base, true, hook, point)
	if err != nil {
		return Published{}, fmt.Errorf("verify and sync prepared Change: %w", err)
	}
	if !manifestsEqual(manifest, actual) {
		return Published{}, &ValidationError{Reason: "prepared tree differs from selected manifest"}
	}
	if err := p.verifyPreparedRoot(); err != nil {
		return Published{}, err
	}
	point.step = stepBeforeRename
	if err := checkpoint(ctx, hook, point); err != nil {
		return Published{}, err
	}
	if err := unix.RenameatxNp(p.parentFD, p.staging, p.parentFD, p.target, unix.RENAME_EXCL); err != nil {
		if errors.Is(err, unix.EEXIST) {
			return Published{}, &ConflictError{Target: changePath(p.parent, p.target)}
		}
		// Darwin documents renameatx_np errors as pre-publication failures.
		return Published{}, fmt.Errorf("publish Change without replacement: %w", err)
	}

	unknown := func(cause error) (Published, error) {
		return Published{}, &OutcomeUnknownError{
			Target: changePath(p.parent, p.target), Identity: p.identity,
			Commitment: manifest.Commitment(), Cause: cause,
		}
	}
	point.step = stepAfterRename
	if err := checkpoint(ctx, hook, point); err != nil {
		return unknown(err)
	}
	if err := verifyNamedRoot(p.parentFD, p.target, p.identity); err != nil {
		return unknown(err)
	}
	if err := verifyOpenRoot(p.stageFD, p.identity); err != nil {
		return unknown(err)
	}
	if err := verifyParentPath(p.parent, p.parentID); err != nil {
		return unknown(err)
	}
	post, err := scanTree(ctx, p.stageFD, p.identity.device, manifest.format, manifest.base, false, hook, point)
	if err != nil {
		return unknown(fmt.Errorf("reconstruct published Change: %w", err))
	}
	if !manifestsEqual(manifest, post) {
		return unknown(&ValidationError{Reason: "published tree differs from selected manifest"})
	}
	point.step = stepBeforeParentFsync
	if err := checkpoint(ctx, hook, point); err != nil {
		return unknown(err)
	}
	if err := unix.Fsync(p.parentFD); err != nil {
		return unknown(fmt.Errorf("fsync Change parent after publication: %w", err))
	}
	facts := factsFromManifest(p.identity, post)
	return Published{
		path: changePath(p.parent, p.target), parent: p.parent, target: p.target,
		facts: facts, format: manifest.format, base: manifest.base,
	}, nil
}

// Close closes retained descriptors only. It never removes staging or target.
func (p *Prepared) Close() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed {
		return &LifecycleError{Reason: "close more than once"}
	}
	p.closed = true
	return errors.Join(unix.Close(p.stageFD), unix.Close(p.parentFD))
}

func (p *Prepared) verifyPreparedRoot() error {
	if err := verifyParentFD(p.parentFD, p.parentID); err != nil {
		return err
	}
	if err := verifyParentPath(p.parent, p.parentID); err != nil {
		return err
	}
	if err := verifyOpenRoot(p.stageFD, p.identity); err != nil {
		return unresolved("retained staging root", p.parent, p.staging, p.identity, true, err)
	}
	if err := verifyNamedRoot(p.parentFD, p.staging, p.identity); err != nil {
		return unresolved("declared staging identity", p.parent, p.staging, p.identity, true, err)
	}
	return nil
}

func (p *Prepared) point(step materializeStep, entryPath []byte) materializePoint {
	return materializePoint{
		step: step, parent: p.parent, stagingName: p.staging, targetName: p.target,
		entryPath: bytes.Clone(entryPath),
	}
}

func (p *Prepared) materializeEntry(ctx context.Context, entry Entry, source BlobSource, hook materializeHook) error {
	data, err := source(ctx, entry.oid)
	if err != nil {
		return fmt.Errorf("read selected blob %s: %w", entry.oid.Hex(), err)
	}
	if uint64(len(data)) != entry.size {
		return &ValidationError{Reason: fmt.Sprintf("blob size differs for %q", entry.path)}
	}
	hashPoint := p.point(stepDuringBlobHash, entry.path)
	actualOID, err := hashBlobContext(ctx, entry.oid.format, data, func() error {
		return checkpoint(ctx, hook, hashPoint)
	})
	if err != nil {
		return err
	}
	if !actualOID.equal(entry.oid) {
		return &ValidationError{Reason: fmt.Sprintf("blob object ID differs for %q", entry.path)}
	}
	components := strings.Split(string(entry.path), "/")
	point := p.point(stepBeforeEntryParentOpen, entry.path)
	if err := checkpoint(ctx, hook, point); err != nil {
		return err
	}
	parentFD, closeParent, err := openEntryParent(p.stageFD, p.identity.device, components[:len(components)-1])
	if err != nil {
		return err
	}
	if closeParent {
		defer unix.Close(parentFD)
	}
	point.step = stepBeforeFileCreate
	if err := checkpoint(ctx, hook, point); err != nil {
		return err
	}
	permissions := uint32(0o644)
	if entry.mode == "100755" {
		permissions = 0o755
	}
	fd, err := unix.Openat(parentFD, components[len(components)-1], unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, permissions)
	if err != nil {
		return fmt.Errorf("create prepared file %q: %w", entry.path, err)
	}
	defer unix.Close(fd)
	if err := unix.Fchmod(fd, permissions); err != nil {
		return fmt.Errorf("set prepared file mode %q: %w", entry.path, err)
	}
	point.step = stepBeforeFileWrite
	if err := checkpoint(ctx, hook, point); err != nil {
		return err
	}
	for written := 0; written < len(data); {
		if err := ctx.Err(); err != nil {
			return err
		}
		end := min(written+(1<<20), len(data))
		n, err := unix.Write(fd, data[written:end])
		if err != nil {
			return fmt.Errorf("write prepared file %q: %w", entry.path, err)
		}
		if n == 0 {
			return fmt.Errorf("write prepared file %q: %w", entry.path, io.ErrShortWrite)
		}
		written += n
	}
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return fmt.Errorf("stat prepared file %q: %w", entry.path, err)
	}
	if stat.Mode&unix.S_IFMT != unix.S_IFREG || uint32(stat.Mode&0o7777) != permissions || stat.Uid != uint32(os.Geteuid()) || stat.Nlink != 1 || stat.Size != int64(entry.size) || uint64(stat.Dev) != p.identity.device {
		return &ValidationError{Reason: fmt.Sprintf("prepared file authority differs for %q", entry.path)}
	}
	point.step = stepBeforeFileFsync
	if err := checkpoint(ctx, hook, point); err != nil {
		return err
	}
	if err := unix.Fsync(fd); err != nil {
		return fmt.Errorf("fsync prepared file %q: %w", entry.path, err)
	}
	return nil
}

// InspectPublished reconstructs one exact recorded target without trusting a
// stored digest. format and base are the durable selected source facts.
func InspectPublished(ctx context.Context, parent, target string, expected StageIdentity, format ObjectFormat, base ObjectID) (TreeFacts, error) {
	return inspectPublished(ctx, parent, target, expected, format, base, nil)
}

// OpenPublished reconstructs one exact recorded target and retains the
// verified directory descriptor for immediate execution. It does not create,
// replace, rename, or remove either the recorded tree or a staging path.
func OpenPublished(ctx context.Context, parent, target string, expected StageIdentity, format ObjectFormat, base ObjectID) (*VerifiedPublished, error) {
	facts, directory, err := inspectPublishedDirectory(ctx, parent, target, expected, format, base, nil)
	if err != nil {
		return nil, err
	}
	return &VerifiedPublished{state: &verifiedPublishedState{directory: directory, facts: facts}}, nil
}

func inspectPublished(ctx context.Context, parent, target string, expected StageIdentity, format ObjectFormat, base ObjectID, hook materializeHook) (TreeFacts, error) {
	facts, directory, err := inspectPublishedDirectory(ctx, parent, target, expected, format, base, hook)
	if directory != nil {
		_ = directory.Close()
	}
	return facts, err
}

func inspectPublishedDirectory(ctx context.Context, parent, target string, expected StageIdentity, format ObjectFormat, base ObjectID, hook materializeHook) (_ TreeFacts, directory *os.File, _ error) {
	if err := validateParentAndName(parent, target); err != nil {
		return TreeFacts{}, nil, err
	}
	if !expected.valid() || !format.valid() || !base.format.valid() || base.format != format {
		return TreeFacts{}, nil, &ValidationError{Reason: "valid expected identity, format and base are required"}
	}
	if err := checkpoint(ctx, hook, materializePoint{}); err != nil {
		return TreeFacts{}, nil, err
	}
	parentFD, err := openVerifiedParent(parent)
	if err != nil {
		return TreeFacts{}, nil, err
	}
	defer unix.Close(parentFD)
	parentID, err := verifySecureParent(parentFD)
	if err != nil {
		return TreeFacts{}, nil, err
	}
	if err := verifyParentPath(parent, parentID); err != nil {
		return TreeFacts{}, nil, err
	}
	rootFD, err := unix.Openat(parentFD, target, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return TreeFacts{}, nil, unresolved("open recorded tree", parent, target, expected, true, err)
	}
	defer func() {
		if directory == nil {
			_ = unix.Close(rootFD)
		}
	}()
	if err := verifyOpenRoot(rootFD, expected); err != nil {
		return TreeFacts{}, nil, unresolved("inspect recorded tree root", parent, target, expected, true, err)
	}
	if err := verifyNamedRoot(parentFD, target, expected); err != nil {
		return TreeFacts{}, nil, unresolved("inspect recorded tree name", parent, target, expected, true, err)
	}
	manifest, err := scanTree(ctx, rootFD, expected.device, format, base, false, hook, materializePoint{parent: parent, targetName: target})
	if err != nil {
		return TreeFacts{}, nil, err
	}
	if err := verifyOpenRoot(rootFD, expected); err != nil {
		return TreeFacts{}, nil, unresolved("recheck recorded tree root", parent, target, expected, true, err)
	}
	if err := verifyNamedRoot(parentFD, target, expected); err != nil {
		return TreeFacts{}, nil, unresolved("recheck recorded tree name", parent, target, expected, true, err)
	}
	if err := verifyParentPath(parent, parentID); err != nil {
		return TreeFacts{}, nil, err
	}
	directory = os.NewFile(uintptr(rootFD), "verified-published-change")
	if directory == nil {
		return TreeFacts{}, nil, &ValidationError{Reason: "retain verified published Change descriptor"}
	}
	return factsFromManifest(expected, manifest), directory, nil
}

// Reinspect reconstructs this exact published tree through the central scanner
// and retains the same verified directory descriptor for immediate execution.
func (p Published) Reinspect(ctx context.Context) (*VerifiedPublished, error) {
	if ctx == nil || p.path == "" || p.parent == "" || p.target == "" ||
		changePath(p.parent, p.target) != p.path || !p.facts.identity.valid() ||
		!p.format.valid() || !p.base.format.valid() || p.base.format != p.format {
		return nil, &ValidationError{Reason: "published Change capability is invalid"}
	}
	facts, directory, err := inspectPublishedDirectory(ctx, p.parent, p.target, p.facts.identity, p.format, p.base, nil)
	if err != nil {
		return nil, err
	}
	if facts != p.facts {
		_ = directory.Close()
		return nil, &ValidationError{Reason: "published Change facts changed"}
	}
	return &VerifiedPublished{state: &verifiedPublishedState{directory: directory, facts: facts}}, nil
}

// Facts returns the immutable facts reconstructed from the retained directory
// only while that exact descriptor still has valid root authority.
func (verified *VerifiedPublished) Facts() (TreeFacts, error) {
	if verified == nil || verified.state == nil {
		return TreeFacts{}, &LifecycleError{Reason: "use invalid verified publication"}
	}
	verified.state.mu.Lock()
	defer verified.state.mu.Unlock()
	if verified.state.closed || verified.state.directory == nil {
		return TreeFacts{}, &LifecycleError{Reason: "use closed verified publication"}
	}
	if err := verifyOpenRoot(int(verified.state.directory.Fd()), verified.state.facts.identity); err != nil {
		return TreeFacts{}, &ValidationError{Reason: "verified published Change descriptor changed"}
	}
	return verified.state.facts, nil
}

// DuplicateDirectory returns one independently owned descriptor for the exact
// retained root. It never resolves or reopens the published pathname.
func (verified *VerifiedPublished) DuplicateDirectory(ctx context.Context) (*os.File, error) {
	if verified == nil || verified.state == nil || ctx == nil {
		return nil, &LifecycleError{Reason: "use invalid verified publication"}
	}
	verified.state.mu.Lock()
	defer verified.state.mu.Unlock()
	if verified.state.closed || verified.state.directory == nil {
		return nil, &LifecycleError{Reason: "use closed verified publication"}
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	identity := verified.state.facts.identity
	if err := verifyOpenRoot(int(verified.state.directory.Fd()), identity); err != nil {
		return nil, &ValidationError{Reason: "verified published Change descriptor changed"}
	}
	fd, err := unix.FcntlInt(verified.state.directory.Fd(), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		return nil, &ValidationError{Reason: "duplicate verified published Change descriptor"}
	}
	duplicate := os.NewFile(uintptr(fd), "verified-published-change-duplicate")
	if duplicate == nil {
		_ = unix.Close(fd)
		return nil, &ValidationError{Reason: "duplicate verified published Change descriptor"}
	}
	if err := ctx.Err(); err != nil {
		_ = duplicate.Close()
		return nil, err
	}
	if err := verifyOpenRoot(int(duplicate.Fd()), identity); err != nil {
		_ = duplicate.Close()
		return nil, &ValidationError{Reason: "duplicated published Change descriptor changed"}
	}
	if err := verifyOpenRoot(int(verified.state.directory.Fd()), identity); err != nil {
		_ = duplicate.Close()
		return nil, &ValidationError{Reason: "verified published Change descriptor changed"}
	}
	return duplicate, nil
}

// Close releases the retained directory descriptor. It never mutates a path.
func (verified *VerifiedPublished) Close() error {
	if verified == nil || verified.state == nil {
		return &LifecycleError{Reason: "close invalid verified publication"}
	}
	verified.state.mu.Lock()
	defer verified.state.mu.Unlock()
	if verified.state.closed || verified.state.directory == nil {
		return &LifecycleError{Reason: "close verified publication more than once"}
	}
	verified.state.closed = true
	err := verified.state.directory.Close()
	verified.state.directory = nil
	return err
}

// RemoveRecordedTree removes only an absent or exact identity-matched recorded
// tree. Under the documented cooperative same-UID boundary, identity is checked
// before traversal and again immediately before root unlink.
func RemoveRecordedTree(ctx context.Context, parent, name string, expected StageIdentity) error {
	return removeRecordedTree(ctx, parent, name, expected, nil)
}

func removeRecordedTree(ctx context.Context, parent, name string, expected StageIdentity, hook materializeHook) error {
	return removeRecordedTreeWithCensus(ctx, parent, name, expected, hook, nil)
}

func removeRecordedTreeWithCensus(ctx context.Context, parent, name string, expected StageIdentity, hook materializeHook, census *removalCensus) error {
	if err := validateParentAndName(parent, name); err != nil {
		return err
	}
	if !expected.valid() {
		return &ValidationError{Reason: "recorded tree identity is required"}
	}
	parentFD, err := openVerifiedParent(parent)
	if err != nil {
		return err
	}
	defer unix.Close(parentFD)
	parentID, err := verifySecureParent(parentFD)
	if err != nil {
		return err
	}
	if err := verifyParentPath(parent, parentID); err != nil {
		return err
	}
	exists, err := namedEntryExists(parentFD, name)
	if err != nil {
		return err
	}
	if !exists {
		return nil
	}
	rootFD, err := unix.Openat(parentFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return unresolved("open recorded tree for removal", parent, name, expected, true, err)
	}
	defer unix.Close(rootFD)
	if err := verifyOpenRoot(rootFD, expected); err != nil {
		return unresolved("verify recorded tree for removal", parent, name, expected, true, err)
	}
	point := materializePoint{step: stepBeforeRecordedRemoval, parent: parent, stagingName: name, targetName: name}
	if err := checkpoint(ctx, hook, point); err != nil {
		return unresolved("recorded tree removal checkpoint", parent, name, expected, true, err)
	}
	if err := verifyNamedRoot(parentFD, name, expected); err != nil {
		return unresolved("recorded tree identity before removal", parent, name, expected, true, err)
	}
	if err := removeDirectoryContents(ctx, parentFD, name, rootFD, expected, census); err != nil {
		return unresolved("recorded tree contents removal", parent, name, expected, true, err)
	}
	if err := verifyNamedRoot(parentFD, name, expected); err != nil {
		return unresolved("recorded tree identity before root removal", parent, name, expected, true, err)
	}
	if err := unix.Unlinkat(parentFD, name, unix.AT_REMOVEDIR); err != nil {
		return unresolved("recorded tree root removal", parent, name, expected, true, err)
	}
	if err := unix.Fsync(parentFD); err != nil {
		return unresolved("recorded tree parent fsync", parent, name, expected, true, err)
	}
	return nil
}

func validatePrepareLocator(parent, target, staging string) error {
	if err := validateParentAndName(parent, target); err != nil {
		return err
	}
	if err := validateName(staging); err != nil {
		return err
	}
	if target == staging {
		return &ValidationError{Reason: "target and staging names must differ"}
	}
	return nil
}

func validateParentAndName(parent, name string) error {
	if !filepath.IsAbs(parent) || filepath.Clean(parent) != parent {
		return &ValidationError{Reason: "Change parent must be one clean absolute path"}
	}
	return validateName(name)
}

func validateName(name string) error {
	if name == "" || name == "." || name == ".." || len(name) > MaxComponentBytes || !utf8.ValidString(name) || strings.ContainsAny(name, "/\\\x00") {
		return &ValidationError{Reason: "Change locator must be one safe name"}
	}
	if strings.EqualFold(name, ".git") {
		return &ValidationError{Reason: ".git locator is forbidden"}
	}
	return nil
}

func checkpoint(ctx context.Context, hook materializeHook, point materializePoint) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if hook != nil {
		if err := hook(point); err != nil {
			return err
		}
	}
	return ctx.Err()
}

func openVerifiedParent(parent string) (int, error) {
	fd, err := unix.Open(parent, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return -1, fmt.Errorf("open verified Change parent: %w", err)
	}
	return fd, nil
}

func verifySecureParent(fd int) (StageIdentity, error) {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return StageIdentity{}, fmt.Errorf("stat Change parent: %w", err)
	}
	if stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Mode&0o7777 != directoryMode || stat.Uid != uint32(os.Geteuid()) {
		return StageIdentity{}, &ValidationError{Reason: "Change parent must be an owned exact mode-0700 directory"}
	}
	return identityOf(stat), nil
}

func verifyParentFD(fd int, expected StageIdentity) error {
	actual, err := verifySecureParent(fd)
	if err != nil {
		return err
	}
	if actual != expected {
		return errors.New("Change parent descriptor identity changed")
	}
	return nil
}

func verifyParentPath(path string, expected StageIdentity) error {
	fd, err := openVerifiedParent(path)
	if err != nil {
		return err
	}
	defer unix.Close(fd)
	actual, err := verifySecureParent(fd)
	if err != nil {
		return err
	}
	if actual != expected {
		return unresolved("verified parent identity", filepath.Dir(path), filepath.Base(path), expected, true, errors.New("Change parent identity changed"))
	}
	return nil
}

func identityOf(stat unix.Stat_t) StageIdentity {
	return StageIdentity{device: uint64(stat.Dev), inode: stat.Ino}
}

func namedIdentity(parentFD int, name string) (StageIdentity, error) {
	var stat unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return StageIdentity{}, err
	}
	return identityOf(stat), nil
}

func namedEntryExists(parentFD int, name string) (bool, error) {
	_, err := namedIdentity(parentFD, name)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, unix.ENOENT) {
		return false, nil
	}
	return false, fmt.Errorf("inspect declared Change path: %w", err)
}

func requireNamedAbsent(parentFD int, parent, name string) error {
	exists, err := namedEntryExists(parentFD, name)
	if err != nil {
		return err
	}
	if exists {
		return &ConflictError{Target: changePath(parent, name)}
	}
	return nil
}

func verifyAdoptableStage(fd int, expected StageIdentity) error {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return err
	}
	if !adoptableStageAuthority(stat, expected, uint32(os.Geteuid())) {
		return errors.New("retained staging authority or identity changed")
	}
	return nil
}

func verifyNamedAdoptableStage(parentFD int, name string, expected StageIdentity) error {
	var stat unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return err
	}
	if !adoptableStageAuthority(stat, expected, uint32(os.Geteuid())) {
		return errors.New("named retained staging authority or identity changed")
	}
	return nil
}

func adoptableStageAuthority(stat unix.Stat_t, expected StageIdentity, effectiveUID uint32) bool {
	// An empty Darwin directory has exactly its name and dot link. More links
	// imply a child directory or authority the adopter did not create.
	return rootAuthority(stat, expected, effectiveUID) && stat.Nlink == 2
}

func verifyOpenRoot(fd int, expected StageIdentity) error {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return err
	}
	if !rootAuthority(stat, expected, uint32(os.Geteuid())) {
		return errors.New("tree root authority or identity changed")
	}
	return nil
}

func verifyOpenIdentity(fd int, expected StageIdentity) error {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return err
	}
	if stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Uid != uint32(os.Geteuid()) || identityOf(stat) != expected {
		return errors.New("opened tree root authority or identity changed")
	}
	return nil
}

func verifyNamedRoot(parentFD int, name string, expected StageIdentity) error {
	var stat unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return err
	}
	if !rootAuthority(stat, expected, uint32(os.Geteuid())) {
		return errors.New("named tree root authority or identity changed")
	}
	return nil
}

func rootAuthority(stat unix.Stat_t, expected StageIdentity, effectiveUID uint32) bool {
	return stat.Mode&unix.S_IFMT == unix.S_IFDIR && stat.Mode&0o7777 == directoryMode &&
		stat.Uid == effectiveUID && identityOf(stat) == expected
}

func unresolved(stage, parent, name string, identity StageIdentity, known bool, cause error) error {
	return &UnresolvedError{Stage: stage, Parent: parent, Name: name, Identity: identity, HasIdentity: known, Cause: cause}
}

func openEntryParent(rootFD int, rootDevice uint64, components []string) (int, bool, error) {
	current := rootFD
	owned := false
	for _, component := range components {
		created := false
		if err := unix.Mkdirat(current, component, directoryMode); err != nil {
			if !errors.Is(err, unix.EEXIST) {
				if owned {
					unix.Close(current)
				}
				return -1, false, fmt.Errorf("create prepared directory %q: %w", component, err)
			}
		} else {
			created = true
		}
		next, err := unix.Openat(current, component, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
		if err != nil {
			if owned {
				unix.Close(current)
			}
			return -1, false, fmt.Errorf("open prepared directory %q without following: %w", component, err)
		}
		if created {
			err = unix.Fchmod(next, directoryMode)
		}
		var stat unix.Stat_t
		if err == nil {
			err = unix.Fstat(next, &stat)
		}
		if err == nil && (stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Mode&0o7777 != directoryMode || stat.Uid != uint32(os.Geteuid()) || uint64(stat.Dev) != rootDevice) {
			err = errors.New("prepared directory authority differs")
		}
		if owned {
			unix.Close(current)
		}
		if err != nil {
			unix.Close(next)
			return -1, false, fmt.Errorf("verify prepared directory %q: %w", component, err)
		}
		current = next
		owned = true
	}
	return current, owned, nil
}

func scanTree(ctx context.Context, rootFD int, rootDevice uint64, format ObjectFormat, base ObjectID, syncDirectories bool, hook materializeHook, point materializePoint) (Manifest, error) {
	entries := make([]Entry, 0)
	directories := make([]string, 0)
	var total uint64
	budget := entryBudget{limit: MaxEntryCount}
	if err := scanDirectory(ctx, rootFD, rootDevice, format, "", &entries, &directories, &total, &budget, syncDirectories, hook, point); err != nil {
		return Manifest{}, err
	}
	manifest, err := NewManifest(format, base, entries)
	if err != nil {
		return Manifest{}, err
	}
	slices.Sort(directories)
	if !slices.Equal(manifest.directories, directories) {
		return Manifest{}, &ValidationError{Reason: "empty or unselected prepared directory exists"}
	}
	return manifest, nil
}

func scanDirectory(ctx context.Context, dirFD int, rootDevice uint64, format ObjectFormat, prefix string, entries *[]Entry, directories *[]string, total *uint64, budget *entryBudget, syncDirectories bool, hook materializeHook, point materializePoint) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	names, err := directoryNames(ctx, dirFD, budget, hook, point)
	if err != nil {
		return err
	}
	for _, name := range names {
		point.step = stepDuringTreeScan
		point.entryPath = []byte(name)
		if err := checkpoint(ctx, hook, point); err != nil {
			return err
		}
		path := name
		if prefix != "" {
			path = prefix + "/" + name
		}
		if _, err := validatePath([]byte(path)); err != nil {
			return err
		}
		var before unix.Stat_t
		if err := unix.Fstatat(dirFD, name, &before, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return err
		}
		if uint64(before.Dev) != rootDevice || before.Uid != uint32(os.Geteuid()) {
			return &ValidationError{Reason: "prepared entry ownership or device differs"}
		}
		switch before.Mode & unix.S_IFMT {
		case unix.S_IFDIR:
			// The daemon makes directories 0700; a provider's mkdir leaves
			// what its umask allows. Either way the owner has everything,
			// nobody else may write, and no special bit is set.
			if before.Mode&0o7022 != 0 || before.Mode&0o700 != 0o700 {
				return &ValidationError{Reason: "prepared directory mode or special bits differ"}
			}
			childFD, err := unix.Openat(dirFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
			if err != nil {
				return err
			}
			var opened unix.Stat_t
			err = unix.Fstat(childFD, &opened)
			if err == nil && identityOf(opened) != identityOf(before) {
				err = errors.New("prepared directory changed while opening")
			}
			if err == nil {
				*directories = append(*directories, path)
				err = scanDirectory(ctx, childFD, rootDevice, format, path, entries, directories, total, budget, syncDirectories, hook, point)
			}
			unix.Close(childFD)
			if err != nil {
				return err
			}
		case unix.S_IFREG:
			// A file the provider made carries what its umask left, 0600
			// under the service's 077; the entry records only whether the
			// owner may execute it, as git does.
			permissions := before.Mode & 0o7777
			if before.Nlink != 1 || permissions&0o7022 != 0 || permissions&0o600 != 0o600 {
				return &ValidationError{Reason: "prepared file link, mode or special bits differ"}
			}
			if before.Size < 0 || uint64(before.Size) > MaxBlobBytes || *total > MaxTotalBlobBytes-uint64(before.Size) {
				return &LimitError{Reason: "reconstructed byte limit exceeded"}
			}
			fd, err := unix.Openat(dirFD, name, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
			if err != nil {
				return err
			}
			var opened unix.Stat_t
			err = unix.Fstat(fd, &opened)
			if err == nil && (identityOf(opened) != identityOf(before) || opened.Mode&unix.S_IFMT != unix.S_IFREG || opened.Nlink != 1) {
				err = errors.New("prepared file changed while opening")
			}
			var oid ObjectID
			if err == nil {
				oid, err = hashOpenBlob(ctx, format, fd, uint64(opened.Size), hook, point)
			}
			unix.Close(fd)
			if err != nil {
				return err
			}
			mode := "100644"
			if permissions&0o100 != 0 {
				mode = "100755"
			}
			entry, err := NewEntry([]byte(path), mode, uint64(opened.Size), oid)
			if err != nil {
				return err
			}
			*entries = append(*entries, entry)
			*total += uint64(opened.Size)
		default:
			return &ValidationError{Reason: "non-regular prepared entry forbidden"}
		}
	}
	if syncDirectories {
		point.step = stepDuringTreeFsync
		point.entryPath = []byte(prefix)
		if err := checkpoint(ctx, hook, point); err != nil {
			return err
		}
		if err := unix.Fsync(dirFD); err != nil {
			return err
		}
	}
	return nil
}

func hashOpenBlob(ctx context.Context, format ObjectFormat, fd int, size uint64, hook materializeHook, point materializePoint) (ObjectID, error) {
	hasher := format.newHash()
	_, _ = fmt.Fprintf(hasher, "blob %d\x00", size)
	buffer := make([]byte, 64<<10)
	var read uint64
	for {
		point.step = stepDuringTreeScan
		if err := checkpoint(ctx, hook, point); err != nil {
			return ObjectID{}, err
		}
		n, err := unix.Read(fd, buffer)
		if n > 0 {
			_, _ = hasher.Write(buffer[:n])
			read += uint64(n)
		}
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return ObjectID{}, err
		}
		if n == 0 {
			break
		}
	}
	if read != size {
		return ObjectID{}, &ValidationError{Reason: "file size changed while hashing"}
	}
	return NewObjectID(format, hasher.Sum(nil))
}

type entryBudget struct {
	observed uint64
	limit    uint64
}

func directoryNames(ctx context.Context, dirFD int, budget *entryBudget, hook materializeHook, point materializePoint) ([]string, error) {
	fd, err := unix.Openat(dirFD, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(fd), "Change directory")
	if file == nil {
		unix.Close(fd)
		return nil, errors.New("wrap Change directory descriptor")
	}
	names := make([]string, 0)
	for {
		if err := ctx.Err(); err != nil {
			file.Close()
			return nil, err
		}
		remainingThroughRefusal := budget.limit + 1 - budget.observed
		batch := min(uint64(128), remainingThroughRefusal)
		entries, readErr := file.ReadDir(int(batch))
		for _, entry := range entries {
			point.step = stepDuringDirectoryRead
			point.entryPath = []byte(entry.Name())
			if err := checkpoint(ctx, hook, point); err != nil {
				file.Close()
				return nil, err
			}
			budget.observed++
			if budget.observed > budget.limit {
				file.Close()
				return nil, &LimitError{Reason: "reconstructed total entry count exceeded"}
			}
			names = append(names, entry.Name())
		}
		if errors.Is(readErr, io.EOF) {
			break
		}
		if readErr != nil {
			file.Close()
			return nil, readErr
		}
	}
	if err := file.Close(); err != nil {
		return nil, err
	}
	slices.Sort(names)
	return names, nil
}

func directoryHasEntry(ctx context.Context, dirFD int) (bool, error) {
	_, found, err := firstDirectoryName(ctx, dirFD, nil)
	return found, err
}

func firstDirectoryName(ctx context.Context, dirFD int, census *removalCensus) (string, bool, error) {
	if err := ctx.Err(); err != nil {
		return "", false, err
	}
	fd, err := unix.Openat(dirFD, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return "", false, err
	}
	census.opened()
	file := os.NewFile(uintptr(fd), "Change directory first entry")
	if file == nil {
		closeRemovalFD(fd, census)
		return "", false, errors.New("wrap Change directory descriptor")
	}
	entries, readErr := file.ReadDir(1)
	closeErr := file.Close()
	census.closed()
	if readErr != nil && !errors.Is(readErr, io.EOF) {
		return "", false, readErr
	}
	if closeErr != nil {
		return "", false, closeErr
	}
	if len(entries) == 0 {
		return "", false, nil
	}
	return entries[0].Name(), true, nil
}

func closeRemovalFD(fd int, census *removalCensus) error {
	err := unix.Close(fd)
	census.closed()
	return err
}

func manifestsEqual(left, right Manifest) bool {
	if left.format != right.format || !left.base.equal(right.base) || left.entryCount != right.entryCount || left.blobBytes != right.blobBytes || len(left.entries) != len(right.entries) {
		return false
	}
	for i := range left.entries {
		a, b := left.entries[i], right.entries[i]
		if !bytes.Equal(a.path, b.path) || a.mode != b.mode || a.size != b.size || !a.oid.equal(b.oid) {
			return false
		}
	}
	return true
}

func factsFromManifest(identity StageIdentity, manifest Manifest) TreeFacts {
	return TreeFacts{
		identity: identity, commitment: manifest.Commitment(),
		entryCount: manifest.EntryCount(), blobBytes: manifest.BlobBytes(),
	}
}

type removalCensus struct {
	open    int
	maximum int
}

func (c *removalCensus) opened() {
	if c == nil {
		return
	}
	c.open++
	c.maximum = max(c.maximum, c.open)
}

func (c *removalCensus) closed() {
	if c != nil {
		c.open--
	}
}

// removeDirectoryContents deliberately restarts from the retained root after
// each unlink. This trades cleanup speed for constant directory-name memory
// and at most three recovery-traversal descriptors, regardless of width/depth.
func removeDirectoryContents(ctx context.Context, parentFD int, rootName string, rootFD int, expected StageIdentity, census *removalCensus) error {
	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		if err := verifyOpenRoot(rootFD, expected); err != nil {
			return err
		}
		if err := verifyNamedRoot(parentFD, rootName, expected); err != nil {
			return err
		}
		empty, err := removeOneRecordedEntry(ctx, rootFD, expected.device, census)
		if err != nil {
			return err
		}
		if empty {
			return nil
		}
	}
}

func removeOneRecordedEntry(ctx context.Context, rootFD int, rootDevice uint64, census *removalCensus) (bool, error) {
	currentFD := rootFD
	currentOwned := false
	defer func() {
		if currentOwned {
			closeRemovalFD(currentFD, census)
		}
	}()

	name, found, err := firstDirectoryName(ctx, currentFD, census)
	if err != nil || !found {
		return !found, err
	}
	for {
		if err := ctx.Err(); err != nil {
			return false, err
		}
		var before unix.Stat_t
		if err := unix.Fstatat(currentFD, name, &before, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return false, err
		}
		if uint64(before.Dev) != rootDevice {
			return false, errors.New("refuse removal across filesystem device")
		}
		if before.Mode&unix.S_IFMT != unix.S_IFDIR {
			if err := verifyNamedEntry(currentFD, name, before); err != nil {
				return false, err
			}
			if err := unix.Unlinkat(currentFD, name, 0); err != nil {
				return false, err
			}
			return false, nil
		}

		childFD, err := openRemovalDirectory(currentFD, name, before, rootDevice, census)
		if err != nil {
			return false, err
		}
		childName, childFound, err := firstDirectoryName(ctx, childFD, census)
		if err != nil {
			closeRemovalFD(childFD, census)
			return false, err
		}
		if !childFound {
			closeRemovalFD(childFD, census)
			if err := verifyNamedEntry(currentFD, name, before); err != nil {
				return false, err
			}
			if err := unix.Unlinkat(currentFD, name, unix.AT_REMOVEDIR); err != nil {
				return false, err
			}
			return false, nil
		}
		if currentOwned {
			closeRemovalFD(currentFD, census)
		}
		currentFD, currentOwned = childFD, true
		name = childName
	}
}

func openRemovalDirectory(parentFD int, name string, expected unix.Stat_t, rootDevice uint64, census *removalCensus) (int, error) {
	fd, err := unix.Openat(parentFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return -1, err
	}
	census.opened()
	var opened unix.Stat_t
	if err := unix.Fstat(fd, &opened); err != nil || identityOf(opened) != identityOf(expected) || uint64(opened.Dev) != rootDevice {
		closeRemovalFD(fd, census)
		if err != nil {
			return -1, err
		}
		return -1, errors.New("directory changed during recorded removal")
	}
	return fd, nil
}

func verifyNamedEntry(parentFD int, name string, expected unix.Stat_t) error {
	var actual unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &actual, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return err
	}
	if identityOf(actual) != identityOf(expected) || actual.Mode&unix.S_IFMT != expected.Mode&unix.S_IFMT {
		return errors.New("entry changed before recorded removal")
	}
	return nil
}
