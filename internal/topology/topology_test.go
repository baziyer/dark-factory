package topology

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestBuildDiscoversGenericGoAndJavaScriptTopologies(t *testing.T) {
	tests := []struct {
		name      string
		files     map[string]string
		nodes     []nodeKey
		languages map[string]string
		imports   [][2]string
	}{
		{
			name:  "unknown repository",
			files: map[string]string{"docs/readme.txt": "notes", "src/main.py": "print('hello')"},
			nodes: []nodeKey{{NodeRepository, "."}, {NodeDirectory, "docs"}, {NodeDirectory, "src"}},
		},
		{
			name: "Go modules, packages, and local imports",
			files: map[string]string{
				"go.mod":          "module example.com/cart\n\ngo 1.27\n",
				"cmd/app/main.go": "package main\nimport (\"fmt\"; \"example.com/cart/lib\")\nfunc main(){fmt.Println(lib.Name)}\n",
				"lib/lib.go":      "package lib\nconst Name = \"cart\"\n",
			},
			nodes:     []nodeKey{{NodeRepository, "."}, {NodeModule, "."}, {NodePackage, "cmd/app"}, {NodePackage, "lib"}},
			languages: map[string]string{"cmd/app": "go", "lib": "go"},
			imports:   [][2]string{{"cmd/app", "lib"}},
		},
		{
			name: "JavaScript and TypeScript local dependencies",
			files: map[string]string{
				"package.json":              `{"name":"@cart/app","dependencies":{"@cart/lib":"workspace:*","react":"latest"}}`,
				"index.js":                  "export const app = true\n",
				"packages/lib/package.json": `{"name":"@cart/lib"}`,
				"packages/lib/src/index.ts": "export const lib = true\n",
			},
			nodes:     []nodeKey{{NodeRepository, "."}, {NodePackage, "."}, {NodeDirectory, "packages"}, {NodePackage, "packages/lib"}, {NodeDirectory, "packages/lib/src"}},
			languages: map[string]string{".": "javascript", "packages/lib": "typescript"},
			imports:   [][2]string{{".", "packages/lib"}},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			root := t.TempDir()
			writeFixture(t, root, test.files)
			snapshot, err := Build(context.Background(), root, "", nil)
			if err != nil {
				t.Fatal(err)
			}
			for _, want := range test.nodes {
				if _, ok := findNode(snapshot, want); !ok {
					t.Errorf("missing %s node %q", want.kind, want.path)
				}
			}
			for rel, language := range test.languages {
				node, ok := findNode(snapshot, nodeKey{NodePackage, rel})
				if !ok || node.Language != language {
					t.Errorf("package %q language = %q, want %q", rel, node.Language, language)
				}
			}
			gotImports := importPaths(snapshot)
			if !equalPairs(gotImports, test.imports) {
				t.Errorf("imports = %v, want %v", gotImports, test.imports)
			}
		})
	}
}

func TestBuildIsStableAndRegeneratesForStructuralChanges(t *testing.T) {
	root := t.TempDir()
	writeFixture(t, root, map[string]string{
		".git/HEAD":  strings.Repeat("a", 40) + "\n",
		"go.mod":     "module example.com/cart\n",
		"app/app.go": "package app\nimport \"example.com/cart/lib\"\n",
		"lib/lib.go": "package lib\n",
		"alt/alt.go": "package alt\n",
	})
	first, err := Build(context.Background(), root, "", nil)
	if err != nil {
		t.Fatal(err)
	}
	if first.SourceRevision != strings.Repeat("a", 40) {
		t.Fatalf("source revision = %q", first.SourceRevision)
	}
	firstJSON, _ := json.Marshal(first)
	second, err := Build(context.Background(), root, "", &first)
	if err != nil {
		t.Fatal(err)
	}
	secondJSON, _ := json.Marshal(second)
	if !bytes.Equal(firstJSON, secondJSON) {
		t.Fatal("unchanged build was not byte-for-byte stable")
	}
	writeFixture(t, root, map[string]string{".git/HEAD": strings.Repeat("b", 40) + "\n"})
	revised, err := Build(context.Background(), root, "", &second)
	if err != nil {
		t.Fatal(err)
	}
	if revised.SourceRevision != strings.Repeat("b", 40) || revised.Digest != second.Digest {
		t.Fatal("source revision changed the graph digest")
	}
	second = revised
	originalIDs := nodeIDs(first)

	writeFixture(t, root, map[string]string{"app/app.go": "package app\nimport \"example.com/cart/alt\"\n"})
	changedImport, err := Build(context.Background(), root, "", &second)
	if err != nil {
		t.Fatal(err)
	}
	if changedImport.Digest == second.Digest || !equalPairs(importPaths(changedImport), [][2]string{{"app", "alt"}}) {
		t.Fatalf("local import change was not reflected: %v", importPaths(changedImport))
	}
	assertStableIDs(t, originalIDs, changedImport)

	writeFixture(t, root, map[string]string{"new/new.go": "package new\n"})
	added, err := Build(context.Background(), root, "", &changedImport)
	if err != nil {
		t.Fatal(err)
	}
	if added.Digest == changedImport.Digest {
		t.Fatal("adding a package did not change the digest")
	}
	assertStableIDs(t, originalIDs, added)
	if err := os.RemoveAll(filepath.Join(root, "new")); err != nil {
		t.Fatal(err)
	}
	removed, err := Build(context.Background(), root, "", &added)
	if err != nil {
		t.Fatal(err)
	}
	if removed.Digest == added.Digest || removed.Digest != changedImport.Digest {
		t.Fatal("removing a package did not restore the prior graph digest")
	}
}

func TestNodeForPathReturnsDeepestKnownNode(t *testing.T) {
	root := t.TempDir()
	writeFixture(t, root, map[string]string{
		"go.mod":     "module example.com/cart\n",
		"root.go":    "package cart\n",
		"app/app.go": "package app\n",
	})
	snapshot, err := Build(context.Background(), root, "", nil)
	if err != nil {
		t.Fatal(err)
	}
	tests := []struct {
		path string
		kind NodeKind
		rel  string
		ok   bool
	}{
		{"./app/app.go", NodePackage, "app", true},
		{"app/not-yet-created/file.go", NodePackage, "app", true},
		{"README.md", NodePackage, ".", true},
		{"../outside", "", "", false},
		{"/outside", "", "", false},
	}
	for _, test := range tests {
		t.Run(test.path, func(t *testing.T) {
			node, ok := NodeForPath(snapshot, test.path)
			if ok != test.ok || ok && (node.Kind != test.kind || node.RelativePath != test.rel) {
				t.Fatalf("NodeForPath() = (%s %q, %t), want (%s %q, %t)", node.Kind, node.RelativePath, ok, test.kind, test.rel, test.ok)
			}
		})
	}
}

func TestBuildIgnoresExcludedAndSymlinkedTreesAndRunsNothing(t *testing.T) {
	root := t.TempDir()
	sentinel := filepath.Join(t.TempDir(), "project-command-ran")
	files := map[string]string{
		"package.json": `{"name":"safe","scripts":{"prepare":"touch ` + sentinel + `"}}`,
		"src/index.ts": "export const safe = true\n",
	}
	for _, ignored := range []string{"node_modules", "vendor", "dist", "build", "target", ".cache", ".next", ".turbo", "coverage", "__pycache__"} {
		files[ignored+"/hidden/package.json"] = `{"name":"ignored-` + ignored + `"}`
	}
	writeFixture(t, root, files)
	if err := os.Symlink(root, filepath.Join(root, "src", "loop")); err != nil {
		t.Fatal(err)
	}
	snapshot, err := Build(context.Background(), root, "", nil)
	if err != nil {
		t.Fatal(err)
	}
	body, err := json.Marshal(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(body, []byte(root)) {
		t.Fatal("snapshot contains its absolute root")
	}
	for _, node := range snapshot.Nodes {
		if filepath.IsAbs(node.RelativePath) || strings.Contains(node.RelativePath, "loop") {
			t.Errorf("unsafe node path %q", node.RelativePath)
		}
		for _, ignored := range []string{"node_modules", "vendor", "dist", "build", "target", ".cache", ".next", ".turbo", "coverage", "__pycache__"} {
			if node.RelativePath == ignored || strings.HasPrefix(node.RelativePath, ignored+"/") {
				t.Errorf("excluded path was discovered: %q", node.RelativePath)
			}
		}
	}
	if _, err := os.Stat(sentinel); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("project command ran: %v", err)
	}
}

// A local toolchain cache under a dot directory and a generated file past the
// analyzer bound are both ordinary facts of a real checkout. Neither may cost
// the whole snapshot: this repository served zero nodes until they stopped
// being fatal.
func TestBuildSkipsDotDirectoriesAndOversizeAnalyzerFiles(t *testing.T) {
	// The root is exempt from the skip, so a checkout that lives under a dot
	// directory still builds its whole structure. Only the walk's own guard on
	// its first entry makes that true.
	root := filepath.Join(t.TempDir(), ".checkout")
	oversize := "package huge\nimport \"example.com/cart/lib\"\n" + strings.Repeat("// pad\n", maxAnalyzerFileBytes/7+1)
	writeFixture(t, root, map[string]string{
		"go.mod": "module example.com/cart\n",
		".tools/local-ci/go-mod/example.com/huge/huge.go": oversize,
		"lib/lib.go":  "package lib\nconst Name = \"cart\"\n",
		"app/app.go":  "package builder\n",
		"app/huge.go": oversize,
	})
	snapshot, err := Build(context.Background(), root, "", nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, node := range snapshot.Nodes {
		if strings.HasPrefix(node.RelativePath, ".tools") {
			t.Errorf("dot directory was discovered: %q", node.RelativePath)
		}
	}
	for _, want := range []nodeKey{{NodeRepository, "."}, {NodeModule, "."}, {NodePackage, "lib"}, {NodePackage, "app"}} {
		if _, ok := findNode(snapshot, want); !ok {
			t.Errorf("missing %s node %q", want.kind, want.path)
		}
	}
	// The unread file names no package either: the directory keeps the name its
	// readable file declares, not the basename an empty vote falls back to.
	if node, _ := findNode(snapshot, nodeKey{NodePackage, "app"}); node.Label != "builder" {
		t.Errorf("package label = %q, want the package clause that was read", node.Label)
	}
	// The import sits in the first bytes of the oversize file, so a truncated
	// read would still find it. Nothing is read: it contributes no imports.
	if got := importPaths(snapshot); len(got) != 0 {
		t.Errorf("imports = %v, want none", got)
	}
}

func TestBuildBoundsFailClearly(t *testing.T) {
	tests := []struct {
		name   string
		files  map[string]string
		bounds limits
		want   string
	}{
		{"depth", map[string]string{"a/b/file": "x"}, limits{depth: 1, files: 10, nodes: 10, bytes: 10}, "depth"},
		{"files", map[string]string{"a": "x", "b": "x"}, limits{depth: 2, files: 1, nodes: 10, bytes: 10}, "file count"},
		{"nodes", map[string]string{"a/file": "x"}, limits{depth: 2, files: 10, nodes: 1, bytes: 10}, "node count"},
		{"bytes", map[string]string{"file": "four"}, limits{depth: 2, files: 10, nodes: 10, bytes: 3}, "byte count"},
		{"containment edges", map[string]string{"a/file": "x", "b/file": "x"}, limits{depth: 2, files: 10, nodes: 10, edges: 1, bytes: 10}, "edge count"},
		{
			"import edges",
			map[string]string{
				"a/package.json": `{"name":"a","dependencies":{"b":"*","c":"*"}}`,
				"b/package.json": `{"name":"b"}`,
				"c/package.json": `{"name":"c"}`,
			},
			limits{depth: 2, files: 10, nodes: 10, edges: 1, bytes: 1 << 20},
			"edge count",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			root := t.TempDir()
			writeFixture(t, root, test.files)
			_, err := build(context.Background(), root, "", nil, test.bounds)
			if !errors.Is(err, ErrBounds) || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("error = %v, want clear %s bound", err, test.want)
			}
		})
	}
}

// The walk visits up to fifty thousand entries on a request with a deadline.
// Without this it runs to completion after the caller has already given up.
func TestBuildStopsWhenTheCallerGivesUp(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "go.mod"), []byte("module example.com/x\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := Build(ctx, root, "", nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled walk error = %v", err)
	}
	if _, err := BuildCached(ctx, root, "", filepath.Join(t.TempDir(), "snapshot.json")); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled cached walk error = %v", err)
	}
}

func TestBuildCachedRegeneratesWithoutRewritingUnchangedCache(t *testing.T) {
	root := t.TempDir()
	writeFixture(t, root, map[string]string{"go.mod": "module example.com/cache\n", "one/one.go": "package one\n"})
	cache := filepath.Join(t.TempDir(), "topology", "project", "snapshot.json")
	first, err := BuildCached(context.Background(), root, "", cache)
	if err != nil {
		t.Fatal(err)
	}
	firstBytes, err := os.ReadFile(cache)
	if err != nil {
		t.Fatal(err)
	}
	firstInfo, err := os.Stat(cache)
	if err != nil {
		t.Fatal(err)
	}
	second, err := BuildCached(context.Background(), root, "", cache)
	if err != nil {
		t.Fatal(err)
	}
	secondBytes, _ := os.ReadFile(cache)
	secondInfo, err := os.Stat(cache)
	if err != nil {
		t.Fatal(err)
	}
	if first.Digest != second.Digest || !bytes.Equal(firstBytes, secondBytes) {
		t.Fatal("unchanged cache was not returned byte-for-byte")
	}
	if !os.SameFile(firstInfo, secondInfo) {
		t.Fatal("unchanged cache was rewritten")
	}
	var forged cacheRecord
	if err := json.Unmarshal(firstBytes, &forged); err != nil {
		t.Fatal(err)
	}
	forged.Snapshot.Nodes[0].Label = "forged"
	forged.Snapshot.Digest = graphDigest(forged.Snapshot.Nodes, forged.Snapshot.Edges)
	forgedBytes, err := json.Marshal(forged)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(cache, forgedBytes, 0o600); err != nil {
		t.Fatal(err)
	}
	repaired, err := BuildCached(context.Background(), root, "", cache)
	if err != nil {
		t.Fatal(err)
	}
	if repaired.Digest != first.Digest || repaired.Nodes[0].Label == "forged" {
		t.Fatal("self-consistent but stale cache graph was trusted")
	}
	writeFixture(t, root, map[string]string{"two/two.go": "package two\n"})
	third, err := BuildCached(context.Background(), root, "", cache)
	if err != nil {
		t.Fatal(err)
	}
	if third.Digest == second.Digest {
		t.Fatal("cached build did not regenerate after package addition")
	}
	if matches, _ := filepath.Glob(filepath.Join(filepath.Dir(cache), ".snapshot-*")); len(matches) != 0 {
		t.Fatalf("temporary cache files remain: %v", matches)
	}
}

type nodeKey struct {
	kind NodeKind
	path string
}

func writeFixture(t *testing.T, root string, files map[string]string) {
	t.Helper()
	for rel, body := range files {
		name := filepath.Join(root, filepath.FromSlash(rel))
		if err := os.MkdirAll(filepath.Dir(name), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(name, []byte(body), 0o600); err != nil {
			t.Fatal(err)
		}
	}
}

func findNode(snapshot Snapshot, key nodeKey) (Node, bool) {
	for _, node := range snapshot.Nodes {
		if node.Kind == key.kind && node.RelativePath == key.path {
			return node, true
		}
	}
	return Node{}, false
}

func nodeIDs(snapshot Snapshot) map[nodeKey]string {
	result := make(map[nodeKey]string, len(snapshot.Nodes))
	for _, node := range snapshot.Nodes {
		result[nodeKey{node.Kind, node.RelativePath}] = node.ID
	}
	return result
}

func assertStableIDs(t *testing.T, before map[nodeKey]string, after Snapshot) {
	t.Helper()
	afterIDs := nodeIDs(after)
	for key, id := range before {
		afterID, ok := afterIDs[key]
		if !ok {
			t.Errorf("stable node %v disappeared", key)
		} else if afterID != id {
			t.Errorf("node %v ID changed from %s to %s", key, id, afterID)
		}
	}
}

func importPaths(snapshot Snapshot) [][2]string {
	paths := make(map[string]string, len(snapshot.Nodes))
	for _, node := range snapshot.Nodes {
		paths[node.ID] = node.RelativePath
	}
	var result [][2]string
	for _, edge := range snapshot.Edges {
		if edge.Kind == EdgeImports {
			result = append(result, [2]string{paths[edge.From], paths[edge.To]})
		}
	}
	return result
}

func equalPairs(left, right [][2]string) bool {
	if len(left) != len(right) {
		return false
	}
	for i := range left {
		if left[i] != right[i] {
			return false
		}
	}
	return true
}

// Two projects holding the same path must never be served the same room id:
// the project is part of every node id, and only the project.
func TestNodeIDsAreMintedPerProject(t *testing.T) {
	root := t.TempDir()
	first, err := Build(context.Background(), root, "project-a", nil)
	if err != nil {
		t.Fatal(err)
	}
	second, err := Build(context.Background(), root, "project-b", nil)
	if err != nil {
		t.Fatal(err)
	}
	again, err := Build(context.Background(), root, "project-a", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(first.Nodes) == 0 || first.Nodes[0].ID == second.Nodes[0].ID || first.Nodes[0].ID != again.Nodes[0].ID {
		t.Fatalf("repository ids: a=%s b=%s a again=%s", first.Nodes[0].ID, second.Nodes[0].ID, again.Nodes[0].ID)
	}
}
