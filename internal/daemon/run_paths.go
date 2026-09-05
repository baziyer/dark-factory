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
// They are vars only so a test can prove each bound without building the tree
// that would trip it; production never assigns them.
var (
	maxRunPaths       = 16
	runPathsWalkLimit = 50_000
)

const runPathsTTL = 5 * time.Second

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
		// A retained change keeps the AvailableAt of its first publication, so
		// a continued run would otherwise report an earlier run's edits. The
		// moment this run started is the later, truthful cut.
		cut := *candidate.Change.AvailableAt
		if candidate.Run.RunningAt != nil && candidate.Run.RunningAt.Int64() > cut.Int64() {
			cut = *candidate.Run.RunningAt
		}
		paths, err := daemon.cachedRunPaths(ctx, candidate.Run.ID, candidate.Change.ID.String(), cut)
		if err != nil {
			return kernel.RunID{}, nil, err
		}
		return candidate.Run.ID, paths, nil
	}
	return kernel.RunID{}, []string{}, nil
}

// cachedRunPaths keeps one walk per run for runPathsTTL. The mutex covers the
// map alone and never the walk, so a cache miss cannot stall RunNext behind a
// directory scan; two racing misses just walk the same tree twice. Expired
// entries are dropped as they are passed, so the map stays as small as the
// live run set without a sweeper goroutine.
func (daemon *Daemon) cachedRunPaths(ctx context.Context, runID kernel.RunID, changeName string, since kernel.UnixMillis) ([]string, error) {
	now := daemon.now()
	if paths, ok := daemon.rememberedRunPaths(runID, now); ok {
		return paths, nil
	}
	parent := daemon.changeParent.Load()
	if parent == nil || *parent == "" {
		return []string{}, nil
	}
	paths := changedDirectories(ctx, filepath.Join(*parent, changeName), time.UnixMilli(since.Int64()))
	if ctx.Err() != nil {
		// A budget that ran out leaves a truncated walk, which must not be
		// cached as this run's answer.
		return nil, ctx.Err()
	}
	daemon.runPathsMu.Lock()
	defer daemon.runPathsMu.Unlock()
	if daemon.runPaths == nil {
		daemon.runPaths = make(map[kernel.RunID]runPathsResult)
	}
	daemon.runPaths[runID] = runPathsResult{at: now, paths: paths}
	return paths, nil
}

func (daemon *Daemon) rememberedRunPaths(runID kernel.RunID, now time.Time) ([]string, bool) {
	daemon.runPathsMu.Lock()
	defer daemon.runPathsMu.Unlock()
	for id, entry := range daemon.runPaths {
		if now.Sub(entry.at) >= runPathsTTL {
			delete(daemon.runPaths, id)
		}
	}
	entry, ok := daemon.runPaths[runID]
	return entry.paths, ok
}

// rememberChangeParent records the one changes root the supervisor was given.
// The daemon does not own the operational home layout and never derives it;
// this is the same value every run in the process is published under, so the
// store is a lock-free publication that RunNext never waits on.
func (daemon *Daemon) rememberChangeParent(parent string) {
	daemon.changeParent.Store(&parent)
}

// changedDirectories returns the deepest directory of every file modified
// after the given moment, deduplicated and sorted. A published change
// directory is a plain materialized tree with no repository metadata, so the
// modification time is the only evidence of the worker's edits. Every stop --
// a missing tree, an unreadable entry, an exhausted budget -- answers with
// what was found rather than an error, so there is nothing to report.
func changedDirectories(ctx context.Context, root string, since time.Time) []string {
	seen := make(map[string]struct{}, maxRunPaths)
	paths := make([]string, 0, maxRunPaths)
	visited := 0
	_ = filepath.WalkDir(root, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			// An unreadable entry is not a reason to refuse the whole answer.
			return nil
		}
		visited++
		if visited > runPathsWalkLimit || len(paths) >= maxRunPaths || ctx.Err() != nil {
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
		if err != nil || !info.ModTime().After(since) {
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
	sort.Strings(paths)
	return paths
}
