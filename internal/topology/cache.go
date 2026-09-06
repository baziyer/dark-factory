package topology

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

const maxCacheBytes = 16 << 20

type cacheRecord struct {
	Fingerprint string   `json:"fingerprint"`
	Snapshot    Snapshot `json:"snapshot"`
}

// BuildCached lazily builds root and atomically replaces cacheFile only when
// its structural fingerprint or source revision changed.
func BuildCached(ctx context.Context, root, project, cacheFile string) (Snapshot, error) {
	previous, err := readCache(cacheFile)
	if err != nil {
		return Snapshot{}, err
	}
	result, err := Build(ctx, root, project, previous)
	if err != nil {
		return Snapshot{}, err
	}
	if previous != nil && result.fingerprint == previous.fingerprint && result.SourceRevision == previous.SourceRevision && result.Digest == previous.Digest {
		return result, nil
	}
	body, err := json.Marshal(cacheRecord{Fingerprint: result.fingerprint, Snapshot: result})
	if err != nil {
		return Snapshot{}, fmt.Errorf("encode topology cache: %w", err)
	}
	if err := replaceCache(cacheFile, append(body, '\n')); err != nil {
		return Snapshot{}, err
	}
	return result, nil
}

func readCache(name string) (*Snapshot, error) {
	body, err := readSmall(name, maxCacheBytes)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if errors.Is(err, ErrBounds) {
		return nil, nil
	}
	if errors.Is(err, errNotRegularFile) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read topology cache: %w", err)
	}
	var record cacheRecord
	if json.Unmarshal(body, &record) != nil || len(record.Fingerprint) != 64 || gitOID(record.Fingerprint) == "" || graphDigest(record.Snapshot.Nodes, record.Snapshot.Edges) != record.Snapshot.Digest {
		return nil, nil
	}
	record.Snapshot.fingerprint = record.Fingerprint
	return &record.Snapshot, nil
}

func replaceCache(name string, body []byte) error {
	directory := filepath.Dir(name)
	if err := os.MkdirAll(directory, 0o700); err != nil {
		return fmt.Errorf("create topology cache directory: %w", err)
	}
	file, err := os.CreateTemp(directory, ".snapshot-*")
	if err != nil {
		return fmt.Errorf("create topology cache: %w", err)
	}
	temporary := file.Name()
	defer os.Remove(temporary)
	if _, err := file.Write(body); err != nil {
		_ = file.Close()
		return fmt.Errorf("write topology cache: %w", err)
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return fmt.Errorf("sync topology cache: %w", err)
	}
	if err := file.Close(); err != nil {
		return fmt.Errorf("close topology cache: %w", err)
	}
	if err := os.Rename(temporary, name); err != nil {
		return fmt.Errorf("replace topology cache: %w", err)
	}
	return nil
}
