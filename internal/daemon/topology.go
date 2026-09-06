package daemon

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/topology"
)

// topologyFreshness bounds how stale a served topology may be. The on-disk
// cache still re-walks the tree to decide whether it changed, which is the
// expensive part, so a repeated request inside this window is answered from
// the last walk instead of walking again.
const topologyFreshness = 30 * time.Second

type topologySnapshot struct {
	snapshot topology.Snapshot
	at       time.Time
}

// ProjectTopology returns the current regenerable topology for an existing
// project without adding it to durable or browser state.
func (daemon *Daemon) ProjectTopology(ctx context.Context, projectID kernel.ProjectID) (topology.Snapshot, error) {
	cacheRoot, err := os.UserCacheDir()
	if err != nil {
		return topology.Snapshot{}, fmt.Errorf("resolve Dark Factory cache directory: %w", err)
	}
	return daemon.projectTopology(ctx, projectID, filepath.Join(cacheRoot, "dark-factory"))
}

func (daemon *Daemon) projectTopology(ctx context.Context, projectID kernel.ProjectID, cacheRoot string) (topology.Snapshot, error) {
	if daemon == nil || daemon.store == nil {
		return topology.Snapshot{}, fmt.Errorf("%w: invalid daemon", kernel.ErrInvalidValue)
	}
	project, found, err := daemon.store.Project(ctx, projectID)
	if err != nil {
		return topology.Snapshot{}, err
	}
	if !found {
		return topology.Snapshot{}, fmt.Errorf("%w: project %s", kernel.ErrNotFound, projectID.String())
	}
	now := daemon.now()
	if fresh, ok := daemon.freshTopology(projectID, now); ok {
		return fresh, nil
	}
	cacheFile := filepath.Join(cacheRoot, "topology", projectID.String(), "snapshot.json")
	// ponytail: the walk is serialized only by each connection's walker, so
	// two connections can walk the same project at once. Add a per-project
	// build gate if that ever costs more than the duplicated walk.
	snapshot, err := topology.BuildCached(ctx, project.Root, projectID.String(), cacheFile)
	if err != nil {
		return topology.Snapshot{}, err
	}
	daemon.topologyMu.Lock()
	if daemon.topologies == nil {
		daemon.topologies = make(map[kernel.ProjectID]topologySnapshot)
	}
	daemon.topologies[projectID] = topologySnapshot{snapshot: snapshot, at: now}
	daemon.topologyMu.Unlock()
	return snapshot, nil
}

func (daemon *Daemon) freshTopology(projectID kernel.ProjectID, now time.Time) (topology.Snapshot, bool) {
	daemon.topologyMu.Lock()
	defer daemon.topologyMu.Unlock()
	held, ok := daemon.topologies[projectID]
	if !ok || now.Before(held.at) || now.Sub(held.at) >= topologyFreshness {
		return topology.Snapshot{}, false
	}
	return held.snapshot, true
}
