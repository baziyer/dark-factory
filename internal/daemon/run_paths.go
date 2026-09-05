package daemon

import (
	"context"
	"fmt"
	"io/fs"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// maxRunPaths is a producer bound, not a wire bound: the console places a
// worker in a handful of rooms. runPathsWalkLimit stops the scan on a tree far
// larger than the console can draw; a truncated answer is the right one, since
// this is a hint about where a worker is, never an authority over the tree.
const (
	maxRunPaths       = 16
	runPathsWalkLimit = 50_000
	runPathsTTL       = 5 * time.Second
)

type runPathsResult struct {
	at    time.Time
	paths []string
}

// RunPaths reports the directories one agent's live run has touched since its
// change directory was published, as paths relative to that directory. An
// agent with no non-terminal run has no run identity and no paths.
func (daemon *Daemon) RunPaths(ctx context.Context, agentID kernel.AgentID) (kernel.RunID, []string, error) {
	if daemon == nil || daemon.store == nil {
		return kernel.RunID{}, nil, fmt.Errorf("%w: invalid daemon", kernel.ErrInvalidValue)
	}
	// ponytail: the store has no agent-to-open-run read, and every non-terminal
	// run is already loaded with its Change here. Add a narrower read only if
	// this shows up in a profile.
	runs, err := daemon.store.RecoverableRuns(ctx)
	if err != nil {
		return kernel.RunID{}, nil, err
	}
	for _, candidate := range runs {
		if candidate.Run.AgentID != agentID || candidate.Change == nil || candidate.Change.AvailableAt == nil {
			continue
		}
		paths, err := daemon.cachedRunPaths(candidate.Run.ID, candidate.Change.ID.String(), *candidate.Change.AvailableAt)
		if err != nil {
			return kernel.RunID{}, nil, err
		}
		return candidate.Run.ID, paths, nil
	}
	return kernel.RunID{}, []string{}, nil
}

// cachedRunPaths keeps one walk per run for runPathsTTL. Expired entries are
// dropped as they are passed, so the map stays as small as the live run set
// without a sweeper goroutine.
func (daemon *Daemon) cachedRunPaths(runID kernel.RunID, changeName string, published kernel.UnixMillis) ([]string, error) {
	daemon.runPathsMu.Lock()
	defer daemon.runPathsMu.Unlock()
	now := daemon.now()
	for id, entry := range daemon.runPaths {
		if now.Sub(entry.at) >= runPathsTTL {
			delete(daemon.runPaths, id)
		}
	}
	if entry, ok := daemon.runPaths[runID]; ok {
		return entry.paths, nil
	}
	if daemon.changeParent == "" {
		return []string{}, nil
	}
	paths, err := changedDirectories(filepath.Join(daemon.changeParent, changeName), time.UnixMilli(published.Int64()))
	if err != nil {
		return nil, err
	}
	if daemon.runPaths == nil {
		daemon.runPaths = make(map[kernel.RunID]runPathsResult)
	}
	daemon.runPaths[runID] = runPathsResult{at: now, paths: paths}
	return paths, nil
}

// rememberChangeParent records the one changes root the supervisor was given.
// The daemon does not own the operational home layout and never derives it;
// this is the same value every run in the process is published under.
func (daemon *Daemon) rememberChangeParent(parent string) {
	daemon.runPathsMu.Lock()
	defer daemon.runPathsMu.Unlock()
	daemon.changeParent = parent
}

// changedDirectories returns the deepest directory of every file modified
// after the tree was published, deduplicated and sorted. A published change
// directory is a plain materialized tree with no repository metadata, so the
// modification time is the only evidence of the worker's edits.
func changedDirectories(root string, published time.Time) ([]string, error) {
	seen := make(map[string]struct{}, maxRunPaths)
	paths := make([]string, 0, maxRunPaths)
	visited := 0
	walkErr := filepath.WalkDir(root, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			// An unreadable entry is not a reason to refuse the whole answer.
			return nil
		}
		visited++
		if visited > runPathsWalkLimit || len(paths) >= maxRunPaths {
			return fs.SkipAll
		}
		if entry.IsDir() {
			if path == root {
				return nil
			}
			switch name := entry.Name(); {
			case strings.HasPrefix(name, "."), name == "node_modules", name == "vendor", name == "target":
				return fs.SkipDir
			}
			return nil
		}
		if !entry.Type().IsRegular() {
			return nil
		}
		info, err := entry.Info()
		if err != nil || !info.ModTime().After(published) {
			return nil
		}
		relative, err := filepath.Rel(root, filepath.Dir(path))
		if err != nil {
			return nil
		}
		if _, ok := seen[relative]; ok {
			return nil
		}
		seen[relative] = struct{}{}
		paths = append(paths, relative)
		return nil
	})
	if walkErr != nil {
		return nil, walkErr
	}
	sort.Strings(paths)
	return paths, nil
}
