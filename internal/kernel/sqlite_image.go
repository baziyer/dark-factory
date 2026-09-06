package kernel

import (
	"bytes"
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"path/filepath"
	"syscall"

	"golang.org/x/sys/unix"
)

const (
	maxFreshDatabaseImageSize     = 8 << 20
	maxImmutableDatabaseImageSize = 256 << 20
	immutableImageReadChunkSize   = 128 << 10
)

type databaseImageScratch struct {
	directory string
	path      string
}

type bootstrapIdentity struct {
	config   FactoryConfig
	at       UnixMillis
	daemonID DaemonID
}

// NewDatabaseImage creates one complete fresh database in a private owner-only
// scratch directory. The returned sidecar-free main file uses SQLite's
// rollback header; Open validates it before promoting it to persistent WAL.
func NewDatabaseImage(ctx context.Context, config FactoryConfig, at UnixMillis) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	config, err := config.normalized()
	if err != nil {
		return nil, err
	}
	var raw [IDBytes]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return nil, fmt.Errorf("generate daemon identifier: %w", err)
	}
	daemonID, err := DaemonIDFromBytes(raw[:])
	if err != nil {
		return nil, err
	}
	image, err := buildDatabaseImage(ctx, bootstrapIdentity{config: config, at: at, daemonID: daemonID})
	if err != nil {
		return nil, err
	}
	if err := InspectPristine(ctx, bytes.NewReader(image), int64(len(image))); err != nil {
		return nil, fmt.Errorf("validate fresh sqlite image: %w", err)
	}
	return image, nil
}

// InspectImmutable validates an already-open, sidecar-free SQLite main file
// without taking ownership of reader. The caller must hold the database
// lifetime lock, prove that no sidecar exists, and keep the declared bytes
// stable until this call returns.
func InspectImmutable(ctx context.Context, reader io.ReaderAt, size int64) error {
	return inspectImmutable(ctx, reader, size, false)
}

// InspectPristine validates the exact sidecar-free rollback image produced by
// NewDatabaseImage. It rebuilds the canonical image from the validated dynamic
// factory identity and requires byte-for-byte equality, so deleted or vacuumed
// history cannot masquerade as a fresh home.
func InspectPristine(ctx context.Context, reader io.ReaderAt, size int64) error {
	return inspectImmutable(ctx, reader, size, true)
}

func buildDatabaseImage(ctx context.Context, identity bootstrapIdentity) (image []byte, resultErr error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	scratch, err := newDatabaseImageScratch(ctx, nil)
	if err != nil {
		return nil, err
	}
	defer func() {
		if err := finishDatabaseImageScratch(scratch.directory, nil, os.RemoveAll); err != nil {
			image = nil
			resultErr = errors.Join(resultErr, err)
		}
	}()

	pool, err := sql.Open(driverName, databaseImageBuildDataSource(scratch.path))
	if err != nil {
		return nil, fmt.Errorf("open private sqlite image: %w", err)
	}
	pool.SetMaxOpenConns(1)
	pool.SetMaxIdleConns(1)
	defer func() {
		if pool != nil {
			if err := pool.Close(); err != nil {
				image = nil
				resultErr = errors.Join(resultErr, fmt.Errorf("close private sqlite image pool: %w", err))
			}
		}
	}()
	connection, err := pool.Conn(ctx)
	if err != nil {
		return nil, fmt.Errorf("checkout private sqlite image connection: %w", err)
	}
	defer func() {
		if connection != nil {
			if err := connection.Close(); err != nil {
				image = nil
				resultErr = errors.Join(resultErr, fmt.Errorf("return private sqlite image connection: %w", err))
			}
		}
	}()
	if _, err := connection.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
		return nil, fmt.Errorf("begin private sqlite image initialization: %w", err)
	}
	if err := initializeFresh(ctx, connection, identity.config, identity.at, identity.daemonID); err != nil {
		_, rollbackErr := connection.ExecContext(context.Background(), "ROLLBACK")
		return nil, errors.Join(err, rollbackErr)
	}
	if _, err := connection.ExecContext(ctx, "COMMIT"); err != nil {
		return nil, fmt.Errorf("commit private sqlite image initialization: %w", err)
	}
	if err := validateDatabaseSnapshot(ctx, connection); err != nil {
		return nil, err
	}
	if err := connection.Close(); err != nil {
		connection = nil
		return nil, fmt.Errorf("return private sqlite image connection: %w", err)
	}
	connection = nil
	if err := pool.Close(); err != nil {
		pool = nil
		return nil, fmt.Errorf("close private sqlite image pool: %w", err)
	}
	pool = nil

	image, err = readScratchImage(ctx, scratch)
	if err != nil {
		return nil, err
	}
	if len(image) == 0 || len(image) > maxFreshDatabaseImageSize {
		return nil, fmt.Errorf("%w: fresh sqlite image size %d outside 1..%d", ErrCorruptState, len(image), maxFreshDatabaseImageSize)
	}
	if err := validateDatabaseHeader(bytes.NewReader(image), int64(len(image)), false); err != nil {
		return nil, err
	}
	if image[18] != 1 || image[19] != 1 {
		return nil, fmt.Errorf("%w: fresh sqlite image must use rollback header 1/1", ErrCorruptState)
	}
	return image, nil
}

func inspectImmutable(ctx context.Context, reader io.ReaderAt, size int64, pristine bool) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if reader == nil || size <= 0 {
		return fmt.Errorf("%w: immutable sqlite reader and positive size are required", ErrInvalidValue)
	}
	if size > maxImmutableDatabaseImageSize {
		return fmt.Errorf("%w: immutable sqlite image size %d exceeds %d", ErrInvalidValue, size, maxImmutableDatabaseImageSize)
	}
	image, err := readImmutableImage(ctx, reader, size)
	if err != nil {
		return err
	}
	if err := validateDatabaseHeader(bytes.NewReader(image), size, false); err != nil {
		return err
	}
	if pristine && (image[18] != 1 || image[19] != 1) {
		return fmt.Errorf("%w: pristine sqlite image must use rollback header 1/1", ErrForeignDatabase)
	}
	identity, err := validateScratchImage(ctx, image, pristine)
	if err != nil || !pristine {
		return err
	}
	canonical, err := buildDatabaseImage(ctx, identity)
	if err != nil {
		return fmt.Errorf("rebuild canonical sqlite image: %w", err)
	}
	if !bytes.Equal(image, canonical) {
		return fmt.Errorf("%w: rollback database is not byte-exact fresh state", ErrForeignDatabase)
	}
	return nil
}

func validateScratchImage(ctx context.Context, image []byte, pristine bool) (identity bootstrapIdentity, resultErr error) {
	scratch, err := newDatabaseImageScratch(ctx, image)
	if err != nil {
		return bootstrapIdentity{}, err
	}
	defer func() {
		resultErr = finishDatabaseImageScratch(scratch.directory, resultErr, os.RemoveAll)
	}()
	pool, err := sql.Open(driverName, databaseImageInspectDataSource(scratch.path))
	if err != nil {
		return bootstrapIdentity{}, fmt.Errorf("open private immutable sqlite database: %w", err)
	}
	pool.SetMaxOpenConns(1)
	pool.SetMaxIdleConns(1)
	defer func() {
		if pool != nil {
			if err := pool.Close(); err != nil {
				resultErr = errors.Join(resultErr, fmt.Errorf("close private immutable sqlite pool: %w", err))
			}
		}
	}()
	connection, err := pool.Conn(ctx)
	if err != nil {
		return bootstrapIdentity{}, fmt.Errorf("checkout private immutable sqlite connection: %w", err)
	}
	defer func() {
		if connection != nil {
			if err := connection.Close(); err != nil {
				resultErr = errors.Join(resultErr, fmt.Errorf("return private immutable sqlite connection: %w", err))
			}
		}
	}()
	if err := validateOpenableSnapshot(ctx, connection); err != nil {
		return bootstrapIdentity{}, err
	}
	if pristine {
		identity, err = readBootstrapIdentity(ctx, connection)
		if err != nil {
			return bootstrapIdentity{}, err
		}
	}
	if err := connection.Close(); err != nil {
		connection = nil
		return bootstrapIdentity{}, fmt.Errorf("return private immutable sqlite connection: %w", err)
	}
	connection = nil
	if err := pool.Close(); err != nil {
		pool = nil
		return bootstrapIdentity{}, fmt.Errorf("close private immutable sqlite pool: %w", err)
	}
	pool = nil
	return identity, nil
}

func newDatabaseImageScratch(ctx context.Context, contents []byte) (_ databaseImageScratch, resultErr error) {
	if err := ctx.Err(); err != nil {
		return databaseImageScratch{}, err
	}
	directory, err := os.MkdirTemp("", "dark-factory-sqlite-image-")
	if err != nil {
		return databaseImageScratch{}, fmt.Errorf("create private sqlite image scratch: %w", err)
	}
	scratch := databaseImageScratch{directory: directory, path: filepath.Join(directory, "image.sqlite3")}
	defer func() {
		if resultErr != nil {
			resultErr = finishDatabaseImageScratch(directory, resultErr, os.RemoveAll)
		}
	}()
	if err := os.Chmod(directory, 0o700); err != nil {
		return databaseImageScratch{}, fmt.Errorf("secure private sqlite image scratch: %w", err)
	}
	if err := validatePrivateDirectory(directory); err != nil {
		return databaseImageScratch{}, err
	}
	file, err := os.OpenFile(scratch.path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return databaseImageScratch{}, fmt.Errorf("create private sqlite image file: %w", err)
	}
	if err := file.Chmod(0o600); err != nil {
		return databaseImageScratch{}, errors.Join(fmt.Errorf("secure private sqlite image file: %w", err), file.Close())
	}
	for offset := 0; offset < len(contents); {
		if err := ctx.Err(); err != nil {
			return databaseImageScratch{}, errors.Join(err, file.Close())
		}
		end := offset + immutableImageReadChunkSize
		if end > len(contents) {
			end = len(contents)
		}
		written, writeErr := file.Write(contents[offset:end])
		if writeErr == nil && written != end-offset {
			writeErr = io.ErrShortWrite
		}
		if writeErr != nil {
			return databaseImageScratch{}, errors.Join(fmt.Errorf("write private sqlite image: %w", writeErr), file.Close())
		}
		offset = end
	}
	if err := file.Close(); err != nil {
		return databaseImageScratch{}, fmt.Errorf("close private sqlite image file: %w", err)
	}
	if err := validateScratchMain(scratch.path, int64(len(contents))); err != nil {
		return databaseImageScratch{}, err
	}
	return scratch, nil
}

func finishDatabaseImageScratch(directory string, cause error, remove func(string) error) error {
	if err := remove(directory); err != nil {
		return errors.Join(cause, fmt.Errorf("remove private sqlite image scratch: %w", err))
	}
	return cause
}

func validateScratchMain(path string, size int64) error {
	info, err := os.Lstat(path)
	if err != nil {
		return fmt.Errorf("inspect private sqlite image file: %w", err)
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if info.Mode() != 0o600 || !ok || stat.Uid != uint32(os.Geteuid()) || stat.Nlink != 1 || info.Size() != size {
		return fmt.Errorf("%w: private sqlite image file is not exact owner-only regular 0600", ErrForeignDatabase)
	}
	return nil
}

func readScratchImage(ctx context.Context, scratch databaseImageScratch) ([]byte, error) {
	entries, err := os.ReadDir(scratch.directory)
	if err != nil {
		return nil, fmt.Errorf("inspect private sqlite image scratch: %w", err)
	}
	if len(entries) != 1 || entries[0].Name() != filepath.Base(scratch.path) {
		return nil, fmt.Errorf("%w: private sqlite image retained unexpected sidecars", ErrCorruptState)
	}
	fd, err := unix.Open(scratch.path, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return nil, fmt.Errorf("open private sqlite image result: %w", err)
	}
	file := os.NewFile(uintptr(fd), scratch.path)
	if file == nil {
		_ = unix.Close(fd)
		return nil, errors.New("open private sqlite image result: invalid file descriptor")
	}
	info, err := file.Stat()
	if err != nil {
		return nil, errors.Join(fmt.Errorf("inspect private sqlite image result: %w", err), file.Close())
	}
	if err := validateScratchMain(scratch.path, info.Size()); err != nil {
		return nil, errors.Join(err, file.Close())
	}
	bound, err := os.Lstat(scratch.path)
	if err != nil || !os.SameFile(info, bound) {
		return nil, errors.Join(fmt.Errorf("%w: private sqlite image path binding changed", ErrCorruptState), err, file.Close())
	}
	if info.Size() <= 0 || info.Size() > maxFreshDatabaseImageSize {
		return nil, errors.Join(fmt.Errorf("%w: private sqlite image size %d outside 1..%d", ErrCorruptState, info.Size(), maxFreshDatabaseImageSize), file.Close())
	}
	image, readErr := readImmutableImage(ctx, file, info.Size())
	return image, errors.Join(readErr, file.Close())
}

func databaseImageBuildDataSource(path string) string {
	query := url.Values{"mode": {"rw"}}
	query["_pragma"] = []string{"foreign_keys(ON)", "journal_mode(DELETE)", "synchronous(FULL)", "temp_store(MEMORY)"}
	return (&url.URL{Scheme: "file", Path: path, RawQuery: query.Encode()}).String()
}

func databaseImageInspectDataSource(path string) string {
	query := url.Values{"mode": {"ro"}, "immutable": {"1"}}
	query["_pragma"] = []string{"foreign_keys(ON)", "query_only(ON)", "temp_store(MEMORY)"}
	return (&url.URL{Scheme: "file", Path: path, RawQuery: query.Encode()}).String()
}

func readImmutableImage(ctx context.Context, reader io.ReaderAt, size int64) ([]byte, error) {
	image := make([]byte, int(size))
	for offset := int64(0); offset < size; {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		end := offset + immutableImageReadChunkSize
		if end > size {
			end = size
		}
		read, err := reader.ReadAt(image[int(offset):int(end)], offset)
		want := int(end - offset)
		if read < 0 || read > want || read != want {
			return nil, fmt.Errorf("read immutable sqlite image at %d: %w", offset, errors.Join(io.ErrUnexpectedEOF, err))
		}
		if err != nil {
			return nil, fmt.Errorf("read immutable sqlite image at %d: %w", offset, err)
		}
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		offset = end
	}
	return image, nil
}

func readBootstrapIdentity(ctx context.Context, connection *sql.Conn) (bootstrapIdentity, error) {
	var daemonBytes []byte
	var dispatch int
	var capacity int64
	var updatedAt int64
	if err := connection.QueryRowContext(ctx, `SELECT daemon_id, dispatch_enabled, capacity, updated_at_ms FROM factory WHERE singleton = 1`).Scan(&daemonBytes, &dispatch, &capacity, &updatedAt); err != nil {
		return bootstrapIdentity{}, fmt.Errorf("read bootstrap identity: %w", err)
	}
	daemonID, err := DaemonIDFromBytes(daemonBytes)
	if err != nil {
		return bootstrapIdentity{}, err
	}
	at, err := NewUnixMillis(updatedAt)
	if err != nil {
		return bootstrapIdentity{}, err
	}
	config, err := (FactoryConfig{DispatchEnabled: dispatch == 1, Capacity: uint16(capacity)}).normalized()
	if err != nil {
		return bootstrapIdentity{}, err
	}
	return bootstrapIdentity{config: config, at: at, daemonID: daemonID}, nil
}

func validateDatabaseHeader(reader io.ReaderAt, size int64, walRequired bool) error {
	header := make([]byte, 100)
	if _, err := reader.ReadAt(header, 0); err != nil {
		return fmt.Errorf("%w: read sqlite header: %w", ErrForeignDatabase, err)
	}
	if err := validateJournalHeaderBytes(header, walRequired); err != nil {
		return err
	}
	pageSize := int64(binary.BigEndian.Uint16(header[16:18]))
	if pageSize == 1 {
		pageSize = 65536
	}
	pageCount := int64(binary.BigEndian.Uint32(header[28:32]))
	if pageSize < 512 || pageSize > 65536 || pageSize&(pageSize-1) != 0 || pageCount < 1 || pageCount > (1<<63-1)/pageSize || pageCount*pageSize != size {
		return fmt.Errorf("%w: invalid sqlite page size or database length", ErrCorruptState)
	}
	return nil
}

func validateJournalHeader(reader io.ReaderAt, walRequired bool) error {
	header := make([]byte, 20)
	if _, err := reader.ReadAt(header, 0); err != nil {
		return fmt.Errorf("%w: read sqlite header: %w", ErrForeignDatabase, err)
	}
	return validateJournalHeaderBytes(header, walRequired)
}

func validateJournalHeaderBytes(header []byte, walRequired bool) error {
	if string(header[:16]) != "SQLite format 3\x00" {
		return fmt.Errorf("%w: invalid sqlite header", ErrForeignDatabase)
	}
	if header[18] != header[19] || header[18] != 1 && header[18] != 2 {
		return fmt.Errorf("%w: invalid database journal header", ErrCorruptState)
	}
	if walRequired && header[18] != 2 {
		return fmt.Errorf("%w: database with WAL sidecars does not have a WAL header", ErrCorruptState)
	}
	return nil
}
